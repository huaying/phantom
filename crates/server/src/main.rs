//! Phantom remote desktop server.
//!
//! Captures the screen (via scrap, NVFBC, PipeWire, or DXGI), encodes it
//! (OpenH264 or NVENC), and streams to connected clients over TCP, QUIC,
//! WebSocket, or WebRTC. Supports encrypted connections, audio capture,
//! bidirectional file transfer, and clipboard synchronization.
//!
//! On Windows, can run as a Windows Service (Session 0) for pre-login access.
//! The service spawns an agent in the user's session for capture; GDI is used
//! as a fallback within the agent when DXGI is unavailable (e.g. lock screen).
//! Use `--install` to register the service, `--uninstall` to remove it.

// Windows FFI stubs (DISPLAY_DEVICEW, DEVMODEW) have no public struct-init
// syntax because of embedded unions and private fields; `default()` + field
// assignment is the conventional idiom and is unavoidable.
#![allow(clippy::field_reassign_with_default)]

// Modules live in lib.rs so integration tests can reach them. Re-alias here
// so the binary's internal `crate::session::...` paths still resolve.
use phantom_server::capture;
#[cfg(target_os = "windows")]
use phantom_server::display_ccd;
use phantom_server::doorbell;
use phantom_server::encode;
#[cfg(target_os = "windows")]
use phantom_server::input_injector;
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
use phantom_server::input_uinput;
#[cfg(target_os = "windows")]
use phantom_server::ipc_pipe;
#[cfg(target_os = "windows")]
use phantom_server::service_win;
use phantom_server::session;
use phantom_server::transport;
use phantom_server::windows_display_policy::WindowsProvisioningMode;
#[cfg(target_os = "windows")]
use phantom_server::windows_display_policy::{
    capture_surface_matches_target, classify_topology, decide_display_provisioning,
    decide_layout_request, external_capture_target_still_valid,
    policy_for as select_windows_display_policy, tier1_adaptive_enabled, Tier1StartupRecovery,
    WindowsAgentDesktop, WindowsCapturePath, WindowsDesktopPhase, WindowsDisplayPolicy,
    WindowsLayoutDecision, WindowsProvisioningDecision, WindowsTopologyKind,
    TIER1_BASELINE_RESOLUTION,
};

#[cfg(target_os = "windows")]
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use phantom_core::crypto;
use phantom_core::encode::{FrameEncoder, VideoCodec};
use phantom_core::frame::Frame;
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
fn instant_ago(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_sub(duration).unwrap_or(now)
}

#[derive(Parser)]
#[command(
    name = "phantom-server",
    version,
    about = "Phantom remote desktop server"
)]
struct Args {
    #[arg(short, long, default_value = "0.0.0.0:9900")]
    listen: String,
    #[arg(short, long, default_value_t = 30)]
    fps: u32,
    #[arg(short, long, default_value_t = 5000)]
    bitrate: u32,
    #[arg(short, long)]
    key: Option<String>,
    #[arg(long)]
    no_encrypt: bool,

    /// Video encoder: auto (default, probes GPU), openh264 (CPU), nvenc (NVIDIA GPU).
    #[arg(long, default_value = "auto")]
    encoder: String,

    /// Video codec: auto (default — H.264 for broad client compat), h264, av1.
    /// AV1 is opt-in only: hardware AV1 decode isn't ubiquitous on clients
    /// yet; software fallback can cause laggy native typing and web tab
    /// OOM crashes. See docs/features.md "AV1 (opt-in, work in progress)".
    #[arg(long, default_value = "auto")]
    codec: String,

    /// Screen capture: auto (default, probes GPU/Wayland), scrap (CPU/X11), nvfbc (NVIDIA GPU), pipewire (Wayland).
    #[arg(long, default_value = "auto")]
    capture: String,

    /// Transport protocol(s), comma-separated: tcp, web, quic.
    /// Default: tcp,web (listens on both TCP and HTTPS/WebSocket).
    #[arg(long, default_value = "tcp,web")]
    transport: String,

    /// Display index to capture (0 = primary). Use --list-displays to see available displays.
    #[arg(long, default_value_t = 0)]
    display: usize,

    /// List available displays and exit.
    #[arg(long)]
    list_displays: bool,

    /// (Windows only) Print VDD/topology/mode diagnostics and exit.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    display_diagnostics: bool,

    /// (Windows only) Provision the installed VDD as the sole 1920x1080
    /// desktop. Intended for dedicated/headless hosts, not workstations.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    provision_vdd_display: bool,

    /// Probe capture + encode once and exit. Intended for installers and
    /// health checks that need to distinguish "display exists" from "a real,
    /// non-black frame can be captured".
    #[arg(long)]
    probe_capture: bool,

    /// Install as auto-start. Windows: registers the Phantom Windows Service
    /// (LocalSystem, auto start) and installs the Virtual Display Driver.
    /// Linux: writes a systemd --user unit and enables it. The blessed Linux
    /// path for end users is the XDG autostart entry written by install.sh;
    /// use --install only when running manually outside of that flow.
    #[arg(long)]
    install: bool,

    /// (Windows only) Manage this dedicated host as one VDD-backed desktop.
    /// Provisioning is transactional and runs before capture, never as a
    /// fallback after a capture failure.
    #[arg(long, requires = "install")]
    managed_display: bool,

    /// (Windows only) Automatically adopt a sole working display, or provision
    /// Phantom's VDD when the host is headless, Basic-only, or multi-display.
    #[arg(long, requires = "install", conflicts_with = "managed_display")]
    auto_display: bool,

    /// Remove auto-start registration (counterpart to --install).
    #[arg(long)]
    uninstall: bool,

    /// (Windows only) Re-run just the Virtual Display Driver install step
    /// against `C:\Program Files\Phantom`. Use this when --install's VDD step
    /// failed on a transient network blip — avoids a full uninstall/install
    /// cycle. No-op on non-Windows.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    install_vdd: bool,

    /// (Windows only) Remove only the Virtual Display Driver and leave the
    /// phantom-server service alone. Use this when you actually want VDD
    /// gone — `--uninstall` no longer touches VDD by default because
    /// removing + reinstalling it on every upgrade was shunting user
    /// windows off-screen.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    uninstall_vdd: bool,

    /// (Windows only, SSO) Install the Phantom Credential Provider DLL.
    /// Copies phantom_cp.dll to System32 and registers CLSID so LogonUI
    /// picks it up. Expects phantom_cp.dll + phantom_cp.reg next to
    /// phantom-server.exe (or pass --cp-dll / --cp-reg to override).
    #[cfg(all(target_os = "windows", feature = "sso"))]
    #[arg(long)]
    install_cp: bool,

    /// (Windows only, SSO) Remove the Phantom Credential Provider.
    /// Unregisters CLSID and deletes the DLL from System32.
    #[cfg(all(target_os = "windows", feature = "sso"))]
    #[arg(long)]
    uninstall_cp: bool,

    /// Override path to phantom_cp.dll for --install-cp.
    #[cfg(all(target_os = "windows", feature = "sso"))]
    #[arg(long)]
    cp_dll: Option<std::path::PathBuf>,

    /// Override path to phantom_cp.reg for --install-cp.
    #[cfg(all(target_os = "windows", feature = "sso"))]
    #[arg(long)]
    cp_reg: Option<std::path::PathBuf>,

    /// Send a file to the first client that connects.
    #[arg(long)]
    send_file: Option<String>,

    /// STUN server for NAT discovery (e.g. stun.l.google.com:19302).
    /// Use "auto" to use Google's public STUN server.
    /// Discovers the server's public IP and prints a connection code.
    /// Note: port forwarding must be set up for the listen port.
    #[arg(long)]
    stun: Option<String>,

    /// Override public address (skip STUN discovery). Format: IP:port.
    #[arg(long)]
    public_addr: Option<String>,

    /// HMAC-SHA256 shared secret (hex-encoded) for JWT token authentication.
    /// When set, WebSocket clients must provide a valid JWT via ?token= query param.
    /// The JWT is signed by an external platform (e.g. CloudStack, Horde).
    #[arg(long)]
    auth_secret: Option<String>,

    /// Run as agent process (launched by service in user session).
    /// Handles DXGI capture + input injection, connects back to service.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    agent_mode: bool,

    /// Windows session ID for IPC pipe isolation (passed by service to agent).
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    ipc_session: Option<u32>,

    /// Monotonic agent generation used to isolate overlapping IPC handoffs.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    ipc_generation: Option<u64>,

    /// Desktop assigned to this Windows agent generation.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    ipc_desktop: Option<String>,

    /// Run as Windows Service (invoked by SCM — do not use manually).
    /// Use `--install` to register the service instead.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    service: bool,

    /// Write log output to this file (in addition to stdout). Used together
    /// with `--log-rotate`. When unset, logs go only to stdout.
    #[arg(long)]
    log_file: Option<std::path::PathBuf>,

    /// Rotation cadence for `--log-file` (daily, hourly, or never).
    #[arg(long, default_value = "daily")]
    log_rotate: String,

    /// How many rotated files to keep. Older files are deleted.
    #[arg(long, default_value_t = 7)]
    log_keep: usize,
}

/// Hold-onto guards returned by tracing-appender so the background flush
/// thread sticks around for the process lifetime. Dropping this aborts the
/// flush thread and you'll lose buffered log lines.
struct LogGuards {
    _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Initialise tracing with stdout output and (optionally) a rotating file
/// sink. Falls back to stdout-only if the file path can't be opened.
fn init_tracing(log_file: &Option<std::path::PathBuf>, rotate: &str, keep: usize) -> LogGuards {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("phantom=info".parse().unwrap());

    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    let (file_layer, _file_guard) = match log_file {
        Some(path) => {
            // Split path into dir + file prefix for tracing-appender.
            let dir = path.parent().unwrap_or(std::path::Path::new("."));
            let file_name = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "phantom.log".into());
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "warning: cannot create log directory {}: {e} — falling back to stdout-only",
                    dir.display()
                );
                (None, None)
            } else {
                let rotation = match rotate {
                    "hourly" => tracing_appender::rolling::Rotation::HOURLY,
                    "never" => tracing_appender::rolling::Rotation::NEVER,
                    _ => tracing_appender::rolling::Rotation::DAILY,
                };
                let appender = tracing_appender::rolling::Builder::new()
                    .rotation(rotation)
                    .filename_prefix(&file_name)
                    .max_log_files(keep)
                    .build(dir)
                    .map_err(|e| {
                        eprintln!("warning: cannot init log file {}: {e}", path.display())
                    });
                match appender {
                    Ok(appender) => {
                        let (nb, guard) = tracing_appender::non_blocking(appender);
                        let layer = tracing_subscriber::fmt::layer()
                            .with_writer(nb)
                            .with_ansi(false);
                        (Some(layer), Some(guard))
                    }
                    Err(_) => (None, None),
                }
            }
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    if log_file.is_some() && _file_guard.is_some() {
        tracing::info!(
            path = %log_file.as_ref().unwrap().display(),
            rotate,
            keep,
            "log file enabled"
        );
    }

    LogGuards { _file_guard }
}

type ConnectionPair = (Box<dyn MessageSender>, Box<dyn MessageReceiver>);
type PendingConnection = (
    Box<dyn MessageSender>,
    Box<dyn MessageReceiver>,
    Option<[u8; 16]>,
);

fn main() -> Result<()> {
    let args = Args::parse();

    // rustls 0.23 requires explicit CryptoProvider install before any TLS use.
    // Without this, `ServerConnection::new()` fails silently and TCP connections
    // get reset during TLS handshake. Must run before service/agent dispatch
    // because the service path also creates TLS connections.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ── Windows: agent/service modes need early detection before tracing init ──
    #[cfg(target_os = "windows")]
    {
        if args.service {
            // Service mode: tracing will be set up by the service itself.
            // Initialize console tracing for the SCM dispatcher.
            let _guards = init_tracing(&args.log_file, &args.log_rotate, args.log_keep);
            tracing::info!("Entering Windows Service dispatcher mode");
            return service_win::run_as_service()
                .map_err(|e| anyhow::anyhow!("service dispatcher failed: {e}"));
        }

        if args.agent_mode {
            // Agent mode: no console, write tracing output to a log file
            // in the system temp directory.
            return run_agent_mode(
                args.ipc_session,
                args.ipc_generation,
                args.ipc_desktop.as_deref(),
            );
        }
    }

    // ── Normal console mode: tracing to stdout (+optional file) ────────────
    let _log_guards = init_tracing(&args.log_file, &args.log_rotate, args.log_keep);

    // ── Graceful shutdown signal (Ctrl+C / SIGTERM) ─────────────────────────
    let shutdown = Arc::new(AtomicBool::new(false));
    // We'll register the session cancel flag later so the signal handler
    // can also cancel an active session immediately.
    let shutdown_cancel: Arc<std::sync::Mutex<Option<Arc<AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(None));
    {
        let shutdown = Arc::clone(&shutdown);
        let shutdown_cancel = Arc::clone(&shutdown_cancel);
        ctrlc::set_handler(move || {
            if shutdown.swap(true, Ordering::SeqCst) {
                // Second signal → force exit immediately
                eprintln!("\nForced exit.");
                std::process::exit(1);
            }
            eprintln!("\nShutting down (press Ctrl+C again to force)...");
            // Cancel any active session so it exits promptly
            if let Some(ref cancel) = *shutdown_cancel.lock().unwrap() {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .expect("failed to set Ctrl+C handler");
    }

    if args.list_displays {
        match capture::scrap::ScrapCapture::list_displays() {
            Ok(displays) => {
                if displays.is_empty() {
                    println!("No displays found.");
                } else {
                    println!("Available displays:");
                    for d in &displays {
                        println!("  {d}");
                    }
                    println!("\nUse --display N to capture a specific display.");
                }
            }
            Err(e) => {
                eprintln!("Failed to enumerate displays: {e}");
            }
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    if args.display_diagnostics {
        return print_windows_display_diagnostics();
    }

    #[cfg(target_os = "windows")]
    if args.provision_vdd_display {
        return provision_windows_vdd_display();
    }

    if args.probe_capture {
        return run_capture_probe(&args);
    }

    if args.install {
        let display_mode = if args.managed_display {
            WindowsProvisioningMode::ManagedVdd
        } else if args.auto_display {
            WindowsProvisioningMode::Auto
        } else {
            WindowsProvisioningMode::PreserveConsole
        };
        return install_autostart(display_mode);
    }
    if args.uninstall {
        return uninstall_autostart();
    }

    #[cfg(target_os = "windows")]
    if args.install_vdd {
        let install_dir = std::path::PathBuf::from(r"C:\Program Files\Phantom");
        println!(
            "Re-installing Virtual Display Driver at {}",
            install_dir.display()
        );
        return service_win::install_vdd(&install_dir);
    }
    #[cfg(target_os = "windows")]
    if args.uninstall_vdd {
        let install_dir = std::path::PathBuf::from(r"C:\Program Files\Phantom");
        println!(
            "Removing Virtual Display Driver at {}",
            install_dir.display()
        );
        return service_win::uninstall_vdd(&install_dir);
    }

    #[cfg(all(target_os = "windows", feature = "sso"))]
    if args.install_cp {
        return install_credential_provider(args.cp_dll.as_deref(), args.cp_reg.as_deref());
    }
    #[cfg(all(target_os = "windows", feature = "sso"))]
    if args.uninstall_cp {
        return uninstall_credential_provider();
    }

    let frame_interval = Duration::from_secs_f64(1.0 / args.fps as f64);

    let encryption_key: Option<[u8; 32]> = if args.no_encrypt {
        tracing::warn!("encryption DISABLED");
        None
    } else {
        let key = match &args.key {
            Some(hex) => crypto::parse_key_hex(hex)?,
            None => {
                let hex = crypto::generate_key_hex();
                tracing::info!("generated encryption key:");
                eprintln!("\n  --key {hex}\n");
                crypto::parse_key_hex(&hex)?
            }
        };
        tracing::info!("encryption ENABLED");
        Some(key)
    };

    let gpu_probe = phantom_gpu::probe::probe();
    // `mut` used only on linux/windows fallback paths.
    #[allow(unused_mut)]
    let (mut encoder_name, mut capture_name, video_codec) =
        resolve_media_config(&args, &gpu_probe)?;

    tracing::info!(encoder = %encoder_name, capture = %capture_name, codec = ?video_codec, display = args.display, "configuration resolved");

    // GPU zero-copy pipeline detection
    #[cfg(target_os = "linux")]
    let mut use_gpu_pipeline = capture_name == "nvfbc" && encoder_name == "nvenc";
    #[cfg(target_os = "windows")]
    let mut use_gpu_pipeline = capture_name == "dxgi" && encoder_name == "nvenc";
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let use_gpu_pipeline = false;

    // GPU zero-copy pipeline (Linux: NVFBC→NVENC, Windows: DXGI→NVENC)
    // Falls back gracefully if init fails (e.g. NVFBC not supported on virtual display)
    #[cfg(target_os = "linux")]
    let mut gpu = if use_gpu_pipeline {
        match GpuPipeline::new(args.fps, args.bitrate, video_codec) {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!("GPU pipeline init failed, falling back to CPU: {e}");
                use_gpu_pipeline = false;
                if gpu_probe.has_nvenc {
                    capture_name = "scrap".to_string();
                } else {
                    encoder_name = "openh264".to_string();
                    capture_name = "scrap".to_string();
                }
                tracing::info!(encoder = %encoder_name, capture = %capture_name, "fallback configuration");
                None
            }
        }
    } else {
        None
    };
    #[cfg(target_os = "windows")]
    let mut gpu_win = if use_gpu_pipeline {
        match phantom_gpu::dxgi_nvenc::DxgiNvencPipeline::new(args.fps, args.bitrate) {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!("DXGI pipeline init failed, falling back to CPU: {e}");
                use_gpu_pipeline = false;
                if gpu_probe.has_nvenc {
                    capture_name = "scrap".to_string();
                } else {
                    encoder_name = "openh264".to_string();
                    capture_name = "scrap".to_string();
                }
                tracing::info!(encoder = %encoder_name, capture = %capture_name, "fallback configuration");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _gpu: Option<()> = None;

    let mut capture: Option<Box<dyn phantom_core::capture::FrameCapture>> = if !use_gpu_pipeline {
        Some(create_capture(&capture_name, args.display)?)
    } else {
        None
    };

    let (width, height) = if use_gpu_pipeline {
        #[cfg(target_os = "linux")]
        {
            (gpu.as_ref().unwrap().width, gpu.as_ref().unwrap().height)
        }
        #[cfg(target_os = "windows")]
        {
            (
                gpu_win.as_ref().unwrap().width,
                gpu_win.as_ref().unwrap().height,
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            unreachable!()
        }
    } else {
        capture.as_ref().unwrap().resolution()
    };

    let mut video_encoder: Option<Box<dyn FrameEncoder>> = if !use_gpu_pipeline {
        Some(create_encoder(
            &encoder_name,
            width,
            height,
            args.fps as f32,
            args.bitrate,
            video_codec,
        )?)
    } else {
        None
    };
    let mut differ = TileDiffer::new();

    // ── Transport listeners ─────────────────────────────────────────────────

    let transports: Vec<&str> = args.transport.split(',').map(|s| s.trim()).collect();
    let (conn_tx, conn_rx) = mpsc::channel::<ConnectionPair>();
    // Audio WS receiver, shared across sessions. Set by "web" transport.
    type AudioWsRxShared = Arc<std::sync::Mutex<Option<mpsc::Receiver<transport::ws::WsSender>>>>;
    let mut audio_ws_rx_shared: Option<AudioWsRxShared> = None;

    let base_port: u16 = args
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9900);
    let listen_host: String = args
        .listen
        .rsplit_once(':')
        .map(|x| x.0)
        .unwrap_or("0.0.0.0")
        .to_string();

    // Parse JWT auth secret (hex → bytes)
    let auth_secret: Option<Vec<u8>> = match &args.auth_secret {
        Some(hex) => {
            let bytes: Result<Vec<u8>> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(Into::into))
                .collect();
            let bytes =
                bytes.map_err(|_| anyhow::anyhow!("invalid --auth-secret: expected hex string"))?;
            tracing::info!("JWT authentication ENABLED for WebSocket connections");
            Some(bytes)
        }
        None => None,
    };

    for transport in &transports {
        match *transport {
            "tcp" => {
                let tcp_addr = format!("{listen_host}:{base_port}");
                let tcp_listener = transport::tcp::TcpServerTransport::bind(&tcp_addr)?;
                let tx = conn_tx.clone();
                let enc_key = encryption_key;
                std::thread::Builder::new()
                    .name("tcp-accept".into())
                    .spawn(move || loop {
                        match tcp_listener.accept_tcp() {
                            Ok(conn) => {
                                let pair = if let Some(ref key) = enc_key {
                                    match conn.split_encrypted(key) {
                                        Ok((s, r)) => (
                                            Box::new(s) as Box<dyn MessageSender>,
                                            Box::new(r) as Box<dyn MessageReceiver>,
                                        ),
                                        Err(e) => {
                                            tracing::warn!("TCP encrypted handshake failed: {e}");
                                            continue;
                                        }
                                    }
                                } else {
                                    match conn.split() {
                                        Ok((s, r)) => (
                                            Box::new(s) as Box<dyn MessageSender>,
                                            Box::new(r) as Box<dyn MessageReceiver>,
                                        ),
                                        Err(e) => {
                                            tracing::warn!("TCP split failed: {e}");
                                            continue;
                                        }
                                    }
                                };
                                if tx.send(pair).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("TCP accept error: {e}");
                            }
                        }
                    })?;
            }
            "web" => {
                let web_port = if transports.len() > 1 {
                    base_port + 1
                } else {
                    base_port
                };
                let mut ws_transport = transport::ws::WebServerTransport::start(
                    web_port,
                    web_port + 1,
                    web_port + 2,
                    auth_secret.clone(),
                )?;
                tracing::info!("open https://localhost:{web_port} in browser");
                // Share audio WS receiver with the session loop
                audio_ws_rx_shared = Some(Arc::new(std::sync::Mutex::new(
                    ws_transport.take_audio_ws_rx(),
                )));
                let tx = conn_tx.clone();
                std::thread::Builder::new()
                    .name("web-accept".into())
                    .spawn(move || loop {
                        let result = {
                            #[cfg(feature = "webrtc")]
                            {
                                ws_transport.accept_any()
                            }
                            #[cfg(not(feature = "webrtc"))]
                            {
                                ws_transport.accept_ws()
                            }
                        };
                        match result {
                            Ok(pair) => {
                                if tx.send(pair).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("WebSocket accept error: {e}");
                            }
                        }
                    })?;
            }
            "quic" => {
                let quic_addr = format!("{listen_host}:{base_port}");
                let quic_listener = transport::quic::QuicServerTransport::bind(&quic_addr)?;
                let tx = conn_tx.clone();
                std::thread::Builder::new()
                    .name("quic-accept".into())
                    .spawn(move || loop {
                        match quic_listener.accept() {
                            Ok((s, r)) => {
                                let pair = (
                                    Box::new(s) as Box<dyn MessageSender>,
                                    Box::new(r) as Box<dyn MessageReceiver>,
                                );
                                if tx.send(pair).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("QUIC accept error: {e}");
                            }
                        }
                    })?;
            }
            other => anyhow::bail!("unknown transport '{other}'. Available: tcp, web, quic"),
        }
    }
    drop(conn_tx);

    // Resolve --send-file path once
    let send_file_path = args.send_file.as_ref().map(std::path::PathBuf::from);

    // ── STUN NAT discovery ──────────────────────────────────────────────────
    // STUN discovers the server's public IP. The connection code uses
    // public_ip:listen_port (assumes port forwarding is set up).
    let stun_server = match args.stun.as_deref() {
        Some("auto") => Some("stun.l.google.com:19302"),
        Some(s) => Some(s),
        None => None,
    };
    if let Some(stun_server) = stun_server {
        match phantom_core::stun::discover_public_addr(stun_server) {
            Ok(public_addr) => {
                let public_ip = public_addr.ip();
                tracing::info!(%public_ip, stun_port = %public_addr.port(), "STUN discovery: public IP");
                // Use public IP + server listen port (user must port-forward this port)
                let connection_addr = format!("{public_ip}:{base_port}");
                print_connection_code(&connection_addr);
            }
            Err(e) => {
                tracing::warn!("STUN discovery failed: {e}");
                tracing::warn!("Clients may not be able to connect from outside the LAN");
            }
        }
    } else if let Some(ref public) = args.public_addr {
        print_connection_code(public);
    }

    // ── Main accept loop (with session replacement) ─────────────────────────
    //
    // A "doorbell" thread blocks on conn_rx. When a new client arrives, it
    // parks the connection in `pending` and sets `cancel` so the active
    // session exits within one frame (~33ms). The main loop then picks up
    // the parked connection and starts a new session.

    let conn_rx = Arc::new(std::sync::Mutex::new(conn_rx));
    let pending: Arc<std::sync::Mutex<Option<PendingConnection>>> =
        Arc::new(std::sync::Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    // Active session token for reconnect validation (future: pre-Hello resume)
    let mut _active_session_token: Vec<u8> = Vec::new();

    // Client-id session affinity (mirrors service_win.rs doorbell). Without
    // this, a forgotten browser tab whose WebSocket auto-reconnects every
    // few seconds keeps stealing the session from the real user. With it,
    // the kicked id sits in a bounded ghost set and gets rejected on retry.
    // Resolution-hint pre-flight is omitted on Linux/non-service since
    // there's no VDD to resize — the X server runs at whatever res it
    // already started at.
    let current_client_id: Arc<std::sync::Mutex<Option<[u8; 16]>>> =
        Arc::new(std::sync::Mutex::new(None));
    let ghost_ids: Arc<std::sync::Mutex<std::collections::VecDeque<[u8; 16]>>> =
        Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(doorbell::GHOST_MAX),
        ));

    {
        let conn_rx = Arc::clone(&conn_rx);
        let pending = Arc::clone(&pending);
        let cancel = Arc::clone(&cancel);
        let current_client_id = Arc::clone(&current_client_id);
        let ghost_ids = Arc::clone(&ghost_ids);
        std::thread::Builder::new()
            .name("doorbell".into())
            .spawn(move || loop {
                let pair = { conn_rx.lock().unwrap().recv() };
                match pair {
                    Ok((mut sender, mut receiver)) => {
                        // Read ClientHello with a short timeout. Legacy clients
                        // (pre-feature) don't send one — they get a None id and
                        // are accepted unconditionally (no tracking). New
                        // clients always send one within the first message.
                        let id: Option<[u8; 16]> =
                            match receiver.recv_msg_within(Duration::from_millis(500)) {
                                Ok(Some(phantom_core::protocol::Message::ClientHello {
                                    client_id,
                                    ..
                                })) => Some(client_id),
                                _ => None,
                            };

                        let mut cur = current_client_id.lock().unwrap();
                        let mut ghosts = ghost_ids.lock().unwrap();
                        let decision = doorbell::decide(id, &mut cur, &mut ghosts);
                        drop(cur);
                        drop(ghosts);

                        if matches!(decision, doorbell::DoorbellDecision::Reject) {
                            tracing::info!("Doorbell: rejecting ghost client (already kicked)");
                            let _ = sender.send_msg(&Message::Disconnect {
                                reason: "ghost client rejected".to_string(),
                            });
                            drop(sender);
                            drop(receiver);
                            continue;
                        }

                        // Replace any previously queued (but not yet consumed) connection
                        *pending.lock().unwrap() = Some((sender, receiver, id));
                        cancel.store(true, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            })
            .expect("spawn doorbell thread");
    }

    loop {
        // Check shutdown before waiting for next client
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown signal received, stopping accept loop");
            break;
        }

        tracing::info!("waiting for client...");

        // Block until a connection is available (or shutdown)
        let conn = loop {
            if shutdown.load(Ordering::Relaxed) {
                break None;
            }
            if let Some(conn) = pending.lock().unwrap().take() {
                break Some(conn);
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let (sender, receiver, session_client_id) = match conn {
            Some(c) => c,
            None => {
                tracing::info!("shutdown signal received, stopping accept loop");
                break;
            }
        };

        // Reset cancel for the new session
        cancel.store(false, Ordering::Relaxed);
        let session_cancel = Arc::clone(&cancel);
        // Register with signal handler so Ctrl+C cancels active session
        *shutdown_cancel.lock().unwrap() = Some(Arc::clone(&cancel));

        // No resume check at accept time — client sends Resume after receiving Hello
        // if it wants to reconnect. The session's receive thread handles Resume.
        let is_resume = false;

        #[cfg(target_os = "linux")]
        let result = if let Some(ref mut gpu) = gpu {
            session::run_session_gpu(
                &mut gpu.capture,
                &mut gpu.encoder,
                session::SessionConfig {
                    sender,
                    receiver,
                    frame_interval,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    input_forwarder: None,
                    audio_ws_rx: audio_ws_rx_shared
                        .as_ref()
                        .and_then(|s| s.lock().ok()?.take()),
                    resolution_change_fn: None,
                    paste_fn: None,
                },
            )
        } else {
            session::run_session_cpu(
                &mut **capture.as_mut().unwrap(),
                &mut **video_encoder.as_mut().unwrap(),
                &mut differ,
                session::SessionConfig {
                    sender,
                    receiver,
                    frame_interval,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    input_forwarder: None,
                    audio_ws_rx: audio_ws_rx_shared
                        .as_ref()
                        .and_then(|s| s.lock().ok()?.take()),
                    resolution_change_fn: None,
                    paste_fn: None,
                },
            )
        };
        #[cfg(target_os = "linux")]
        {
            _active_session_token = result.session_token.clone();
            // session end is already logged by make_session_result with
            // structured fields (session_id, reason).
        }
        #[cfg(target_os = "windows")]
        let result = if let Some(ref mut gw) = gpu_win {
            session::run_session_dxgi(
                gw,
                args.bitrate,
                session::SessionConfig {
                    sender,
                    receiver,
                    frame_interval,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    input_forwarder: None,
                    audio_ws_rx: audio_ws_rx_shared
                        .as_ref()
                        .and_then(|s| s.lock().ok()?.take()),
                    resolution_change_fn: None,
                    paste_fn: None,
                },
            )
        } else {
            session::run_session_cpu(
                &mut **capture.as_mut().unwrap(),
                &mut **video_encoder.as_mut().unwrap(),
                &mut differ,
                session::SessionConfig {
                    sender,
                    receiver,
                    frame_interval,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    input_forwarder: None,
                    audio_ws_rx: audio_ws_rx_shared
                        .as_ref()
                        .and_then(|s| s.lock().ok()?.take()),
                    resolution_change_fn: None,
                    paste_fn: None,
                },
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let result = session::run_session_cpu(
            &mut **capture.as_mut().unwrap(),
            &mut **video_encoder.as_mut().unwrap(),
            &mut differ,
            session::SessionConfig {
                sender,
                receiver,
                frame_interval,
                cancel: session_cancel,
                send_file: send_file_path.as_deref(),
                video_codec,
                is_resume,
                input_forwarder: None,
                audio_ws_rx: audio_ws_rx_shared
                    .as_ref()
                    .and_then(|s| s.lock().ok()?.take()),
                resolution_change_fn: None,
                paste_fn: None,
            },
        );

        // Update active session token from session result
        #[cfg(not(target_os = "linux"))]
        {
            _active_session_token = result.session_token.clone();
            // session end is already logged by make_session_result with
            // structured fields (session_id, reason).
        }

        if !matches!(result.reason, Some(session::SessionEndReason::Cancelled)) {
            let mut cur = current_client_id.lock().unwrap();
            if *cur == session_client_id {
                *cur = None;
            }
        }

        // Post-session cleanup
        differ.reset();
        if let Some(ref mut enc) = video_encoder {
            enc.force_keyframe();
        }
        #[cfg(target_os = "linux")]
        if let Some(ref mut gpu) = gpu {
            // Reuse the encoder for unchanged geometry, but reset the ABR
            // baseline. Otherwise each reconnect can multiply its bitrate
            // ceiling from the preceding session's adapted value.
            if let Err(e) = gpu.encoder.set_bitrate_kbps(gpu.bitrate) {
                tracing::warn!("GPU session bitrate reset failed: {e}");
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(ref mut gw) = gpu_win {
            if let Err(e) = gw.reset_for_new_session() {
                tracing::error!("DXGI pipeline reset failed: {e}");
            }
        }
    }

    // ── Shutdown complete ───────────────────────────────────────────────────
    // Set cancel to ensure any lingering session thread exits
    cancel.store(true, Ordering::Relaxed);

    // Give threads a moment to clean up (max 2s)
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::info!("goodbye 👋");
    Ok(())
}

fn resolve_media_config(
    args: &Args,
    gpu_probe: &phantom_gpu::probe::GpuProbeResult,
) -> Result<(String, String, VideoCodec)> {
    #[allow(unused_mut)]
    let mut encoder_name = if args.encoder == "auto" {
        gpu_probe.best_encoder().to_string()
    } else {
        args.encoder.clone()
    };

    let mut capture_name = if args.capture == "auto" {
        // On Wayland sessions, prefer PipeWire capture (if feature enabled).
        #[cfg(feature = "wayland")]
        {
            if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                || std::env::var("WAYLAND_DISPLAY").is_ok()
            {
                tracing::info!("Wayland session detected, using PipeWire capture");
                "pipewire".to_string()
            } else {
                gpu_probe.best_capture().to_string()
            }
        }
        #[cfg(not(feature = "wayland"))]
        {
            gpu_probe.best_capture().to_string()
        }
    } else {
        args.capture.clone()
    };

    // If encoder is explicitly non-GPU but capture resolved to a GPU-only
    // method, fix it. The CPU encoder needs CPU-visible BGRA frames.
    if encoder_name == "openh264" && (capture_name == "nvfbc" || capture_name == "dxgi") {
        tracing::info!(
            "encoder is openh264, overriding capture from {} to scrap",
            capture_name
        );
        capture_name = "scrap".to_string();
    }

    // Default codec selection intentionally picks H.264 even when the GPU can
    // do AV1. Reason: AV1 support on the client side is the weak link —
    // software dav1d on a mid-range Mac / Intel Chrome can cost 20-40 ms per
    // 1080p frame, which shows up as typing lag on the native client and as
    // outright browser-tab OOM crashes on the web client (observed on U22
    // L40). H.264 decodes on every platform we ship with hardware acceleration
    // (VideoToolbox on macOS, NVDEC on Linux/Windows, WebCodecs H.264
    // everywhere).
    //
    // AV1 is kept as an explicit opt-in (`--codec av1`) while we:
    //   (a) teach ClientHello to advertise supported decoders
    //   (b) let the server pick the codec intersection
    // Until (a) + (b) land, defaulting to AV1 is a regression for the majority
    // of clients even when the server supports it.
    let video_codec = match args.codec.as_str() {
        "auto" => {
            tracing::info!(
                "codec=auto -> H.264 (AV1 is opt-in via --codec av1; client decode support still rolling out)"
            );
            VideoCodec::H264
        }
        "h264" | "H264" | "h.264" => VideoCodec::H264,
        "av1" | "AV1" => {
            if encoder_name != "nvenc" {
                anyhow::bail!("AV1 codec requires --encoder nvenc (OpenH264 only supports H.264)");
            }
            // Non-fatal probe: warn if the GPU isn't reporting AV1 but let it
            // through — the NVENC init will surface the real error in a
            // consistent format if it really can't.
            if gpu_probe.best_codec() != "av1" {
                tracing::warn!(
                    "AV1 requested but GPU probe did not confirm AV1 support; \
                     continuing anyway — NVENC will error out cleanly if unsupported"
                );
            }
            VideoCodec::Av1
        }
        other => anyhow::bail!("unknown codec: {other} (supported: auto, h264, av1)"),
    };

    Ok((encoder_name, capture_name, video_codec))
}

fn run_capture_probe(args: &Args) -> Result<()> {
    let gpu_probe = phantom_gpu::probe::probe();
    let (encoder_name, capture_name, video_codec) = resolve_media_config(args, &gpu_probe)?;

    println!("Phantom capture probe:");
    println!(
        "  resolved: capture={} encoder={} codec={:?} display={}",
        capture_name, encoder_name, video_codec, args.display
    );
    println!(
        "  gpu_probe: encoder={} capture={} gpu_pipeline={} gpu={}",
        gpu_probe.best_encoder(),
        gpu_probe.best_capture(),
        gpu_probe.has_gpu_pipeline(),
        gpu_probe.gpu_name.as_deref().unwrap_or("none")
    );

    if capture_name == "nvfbc" && encoder_name == "nvenc" {
        #[cfg(target_os = "linux")]
        {
            match run_linux_nvfbc_probe(args, video_codec) {
                Ok(()) => return Ok(()),
                Err(e) if args.capture == "auto" && args.encoder == "auto" => {
                    println!("  zero_copy: failed: {e:#}");
                    println!("  fallback: trying capture=scrap encoder=nvenc");
                    return run_cpu_visible_capture_probe(
                        "scrap",
                        "nvenc",
                        args.display,
                        args.fps,
                        args.bitrate,
                        video_codec,
                    );
                }
                Err(e) => return Err(e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("NVFBC capture is only available on Linux");
        }
    }

    if capture_name == "dxgi" && encoder_name == "nvenc" {
        #[cfg(target_os = "windows")]
        {
            match run_windows_dxgi_probe(args) {
                Ok(()) => return Ok(()),
                Err(e) if args.capture == "auto" && args.encoder == "auto" => {
                    println!("  dxgi_nvenc: failed: {e:#}");
                    println!("  fallback: trying capture=scrap encoder=nvenc");
                    return run_cpu_visible_capture_probe(
                        "scrap",
                        "nvenc",
                        args.display,
                        args.fps,
                        args.bitrate,
                        video_codec,
                    );
                }
                Err(e) => return Err(e),
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            anyhow::bail!("DXGI capture is only available on Windows");
        }
    }

    run_cpu_visible_capture_probe(
        &capture_name,
        &encoder_name,
        args.display,
        args.fps,
        args.bitrate,
        video_codec,
    )
}

fn run_cpu_visible_capture_probe(
    capture_name: &str,
    encoder_name: &str,
    display: usize,
    fps: u32,
    bitrate: u32,
    video_codec: VideoCodec,
) -> Result<()> {
    let mut capture = create_capture(capture_name, display)?;
    let frame = wait_for_probe_frame(capture.as_mut(), Duration::from_secs(3))?;
    print_frame_probe_stats(&frame)?;

    let mut encoder = create_encoder(
        encoder_name,
        frame.width,
        frame.height,
        fps as f32,
        bitrate,
        video_codec,
    )?;
    encoder.force_keyframe();
    let encoded = encoder.encode_frame(&frame)?;
    if encoded.data.is_empty() {
        anyhow::bail!("encoder produced an empty frame");
    }
    println!(
        "  encode: ok bytes={} keyframe={} codec={:?}",
        encoded.data.len(),
        encoded.is_keyframe,
        encoded.codec
    );
    println!("Capture probe result: pass");
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_dxgi_probe(args: &Args) -> Result<()> {
    use anyhow::Context;

    capture::gdi::switch_to_input_desktop();
    let input_desktop =
        capture::gdi::current_input_desktop_name().unwrap_or_else(|| "unknown".to_string());
    let vdd_device = find_vdd_device_name();
    let target_device = current_primary_display_device_name();
    println!("  windows: input_desktop={input_desktop}");
    println!(
        "  windows: vdd_device={}",
        vdd_device.as_deref().unwrap_or("none")
    );
    println!(
        "  windows: active_capture_target={}",
        target_device.as_deref().unwrap_or("none")
    );

    match display_ccd::active_config_summary() {
        Ok(lines) => {
            println!("  ccd: active_paths={}", lines.len());
            for line in lines {
                println!("  ccd: {line}");
            }
        }
        Err(e) => println!("  ccd: unavailable: {e:#}"),
    }

    if let Ok(displays) = capture::scrap::ScrapCapture::list_displays() {
        for d in displays {
            println!(
                "  display[{}]: {}x{} primary={}",
                d.index, d.width, d.height, d.is_primary
            );
        }
    }

    let target = target_device.as_deref();
    let mut gpu = phantom_gpu::dxgi_nvenc::DxgiNvencPipeline::with_target_device(
        args.fps,
        args.bitrate,
        target,
    )
    .with_context(|| match target {
        Some(t) => format!("DXGI/NVENC active-target probe failed for {t}"),
        None => "DXGI/NVENC generic probe failed; no active primary display was found".to_string(),
    })?;
    println!(
        "  dxgi_nvenc: initialized {}x{} target={}",
        gpu.width,
        gpu.height,
        gpu.capture.target_summary()
    );
    gpu.force_keyframe_with_capture_reset();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(encoded) = gpu.capture_and_encode()? {
            if encoded.data.is_empty() {
                anyhow::bail!("DXGI/NVENC produced an empty encoded frame");
            }
            let stats = gpu.capture.sample_bgra_stats(4096)?;
            println!(
                "  dxgi_nvenc: encoded bytes={} keyframe={} codec={:?}",
                encoded.data.len(),
                encoded.is_keyframe,
                encoded.codec
            );
            println!(
                "  frame: ok {}x{} black_pct={} mean_rgb={},{},{}",
                gpu.width, gpu.height, stats.black_pct, stats.mean_r, stats.mean_g, stats.mean_b
            );
            if stats.is_mostly_black() {
                println!("Capture probe result: mostly-black");
                anyhow::bail!(
                    "DXGI captured frame is mostly black (black_pct={} mean_rgb={},{},{})",
                    stats.black_pct,
                    stats.mean_r,
                    stats.mean_g,
                    stats.mean_b
                );
            }
            println!("Capture probe result: pass");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    anyhow::bail!("DXGI/NVENC timed out waiting for the first encoded frame")
}

#[cfg(target_os = "linux")]
fn run_linux_nvfbc_probe(args: &Args, video_codec: VideoCodec) -> Result<()> {
    let gpu = GpuPipeline::new(args.fps, args.bitrate, video_codec)?;
    println!(
        "  zero_copy: ok {}x{} (nvfbc -> nvenc)",
        gpu.width, gpu.height
    );
    drop(gpu);

    // The zero-copy path proves NVFBC and NVENC can initialize, but it doesn't
    // let the doctor inspect pixels. Open a short-lived BGRA NVFBC session too
    // so black-screen installs fail before a user opens the browser.
    let cuda = std::sync::Arc::new(phantom_gpu::cuda::CudaLib::load()?);
    let dev = cuda.device_get(0)?;
    let primary_ctx = cuda.primary_ctx_retain(dev)?;
    unsafe { cuda.ctx_push(primary_ctx)? };
    let mut capture = phantom_gpu::nvfbc::NvfbcCapture::new(
        std::sync::Arc::clone(&cuda),
        primary_ctx,
        phantom_gpu::sys::NVFBC_BUFFER_FORMAT_BGRA,
    )?;
    let frame = wait_for_probe_frame(&mut capture, Duration::from_secs(3))?;
    let _ = capture.release_context();
    print_frame_probe_stats(&frame)?;
    println!("Capture probe result: pass");
    Ok(())
}

fn wait_for_probe_frame(
    capture: &mut dyn phantom_core::capture::FrameCapture,
    timeout: Duration,
) -> Result<Frame> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(frame) = capture.capture()? {
            return Ok(frame);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    anyhow::bail!("timed out waiting for a capture frame");
}

fn print_frame_probe_stats(frame: &Frame) -> Result<()> {
    let stats = frame_probe_stats(&frame.data);
    println!(
        "  frame: ok {}x{} black_pct={} mean_rgb={},{},{}",
        frame.width, frame.height, stats.black_pct, stats.mean_r, stats.mean_g, stats.mean_b
    );
    if stats.is_mostly_black() {
        println!("Capture probe result: mostly-black");
        anyhow::bail!(
            "captured frame is mostly black (black_pct={} mean_rgb={},{},{})",
            stats.black_pct,
            stats.mean_r,
            stats.mean_g,
            stats.mean_b
        );
    }
    Ok(())
}

struct FrameProbeStats {
    black_pct: u32,
    mean_r: u32,
    mean_g: u32,
    mean_b: u32,
}

impl FrameProbeStats {
    fn is_mostly_black(&self) -> bool {
        self.black_pct >= 99 && self.mean_r < 8 && self.mean_g < 8 && self.mean_b < 8
    }
}

fn frame_probe_stats(data: &[u8]) -> FrameProbeStats {
    if data.len() < 4 {
        return FrameProbeStats {
            black_pct: 100,
            mean_r: 0,
            mean_g: 0,
            mean_b: 0,
        };
    }

    let pixels = data.len() / 4;
    let step = (pixels / 4096).max(1);
    let mut sampled = 0u32;
    let mut black = 0u32;
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;

    for pixel in (0..pixels).step_by(step) {
        let offset = pixel * 4;
        let b = data[offset] as u32;
        let g = data[offset + 1] as u32;
        let r = data[offset + 2] as u32;
        sampled += 1;
        sum_r += r as u64;
        sum_g += g as u64;
        sum_b += b as u64;
        if r < 8 && g < 8 && b < 8 {
            black += 1;
        }
    }

    if sampled == 0 {
        return FrameProbeStats {
            black_pct: 100,
            mean_r: 0,
            mean_g: 0,
            mean_b: 0,
        };
    }

    FrameProbeStats {
        black_pct: black * 100 / sampled,
        mean_r: (sum_r / sampled as u64) as u32,
        mean_g: (sum_g / sampled as u64) as u32,
        mean_b: (sum_b / sampled as u64) as u32,
    }
}

// ── GPU pipeline struct (Linux) ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct GpuPipeline {
    capture: phantom_gpu::nvfbc::NvfbcCapture,
    encoder: phantom_gpu::nvenc::NvencEncoder,
    width: u32,
    height: u32,
    bitrate: u32,
}

#[cfg(target_os = "linux")]
impl GpuPipeline {
    fn new(fps: u32, bitrate_kbps: u32, codec: VideoCodec) -> Result<Self> {
        use phantom_core::capture::FrameCapture;
        let cuda = std::sync::Arc::new(phantom_gpu::cuda::CudaLib::load()?);
        let dev = cuda.device_get(0)?;
        let primary_ctx = cuda.primary_ctx_retain(dev)?;
        unsafe { cuda.ctx_push(primary_ctx)? };

        let mut capture = phantom_gpu::nvfbc::NvfbcCapture::new(
            std::sync::Arc::clone(&cuda),
            primary_ctx,
            phantom_gpu::sys::NVFBC_BUFFER_FORMAT_NV12,
        )?;
        let (sw, sh) = capture.resolution();

        let first_deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            if Instant::now() >= first_deadline {
                anyhow::bail!("NVFBC initial grab timed out waiting for the first frame");
            }
            std::thread::sleep(Duration::from_millis(20));
            match capture.grab_cuda() {
                Ok(Some(f)) => break f,
                Ok(None) => continue,
                Err(e) => anyhow::bail!("NVFBC initial grab failed: {e}"),
            }
        };
        let (width, height) = (first.width, first.height);
        tracing::info!(
            screen_w = sw,
            screen_h = sh,
            width,
            height,
            "NVFBC→NVENC GPU pipeline"
        );

        capture.release_context()?;
        let encoder = unsafe {
            phantom_gpu::nvenc::NvencEncoder::with_context(
                std::sync::Arc::clone(&cuda),
                primary_ctx,
                false,
                width,
                height,
                fps,
                bitrate_kbps,
                codec,
            )?
        };

        Ok(Self {
            capture,
            encoder,
            width,
            height,
            bitrate: bitrate_kbps,
        })
    }
}

// ── Factory functions ───────────────────────────────────────────────────────

fn create_capture(
    name: &str,
    display_index: usize,
) -> Result<Box<dyn phantom_core::capture::FrameCapture>> {
    match name {
        "scrap" => {
            let cap = capture::scrap::ScrapCapture::with_display(display_index)?;
            Ok(Box::new(cap))
        }
        #[cfg(feature = "wayland")]
        "pipewire" => {
            if display_index != 0 {
                tracing::warn!(
                    "PipeWire capture: --display is ignored (portal handles display selection)"
                );
            }
            let cap = capture::pipewire::PipeWireCapture::new()?;
            Ok(Box::new(cap))
        }
        other => {
            let available = if cfg!(feature = "wayland") {
                "scrap, pipewire, nvfbc"
            } else {
                "scrap, nvfbc (use with --encoder nvenc for GPU pipeline)"
            };
            anyhow::bail!("unknown capture '{other}'. Available: {available}")
        }
    }
}

fn create_encoder(
    name: &str,
    width: u32,
    height: u32,
    fps: f32,
    bitrate_kbps: u32,
    codec: VideoCodec,
) -> Result<Box<dyn FrameEncoder>> {
    match name {
        "openh264" => {
            if codec == VideoCodec::Av1 {
                anyhow::bail!("OpenH264 does not support AV1. Use --encoder nvenc for AV1.");
            }
            let enc = encode::h264::OpenH264Encoder::new(width, height, fps, bitrate_kbps)?;
            Ok(Box::new(enc))
        }
        "nvenc" => {
            let cuda = std::sync::Arc::new(phantom_gpu::cuda::CudaLib::load()?);
            let enc = phantom_gpu::nvenc::NvencEncoder::new(
                cuda,
                0,
                width,
                height,
                fps as u32,
                bitrate_kbps,
                codec,
            )?;
            Ok(Box::new(enc))
        }
        other => anyhow::bail!("unknown encoder '{other}'. Available: openh264, nvenc"),
    }
}

// ---------------------------------------------------------------------------
// SSO Credential Provider install/uninstall (Windows)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "windows", feature = "sso"))]
const CP_CLSID: &str = "{ccd145e9-71bb-4e91-a604-2ee449adfd54}";

#[cfg(all(target_os = "windows", feature = "sso"))]
fn install_credential_provider(
    dll_override: Option<&std::path::Path>,
    reg_override: Option<&std::path::Path>,
) -> Result<()> {
    use anyhow::Context;

    let exe_dir = std::env::current_exe()
        .context("current_exe")?
        .parent()
        .context("exe has no parent")?
        .to_path_buf();

    let dll_src: std::path::PathBuf = dll_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| exe_dir.join("phantom_cp.dll"));
    let reg_src: std::path::PathBuf = reg_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| exe_dir.join("phantom_cp.reg"));

    if !dll_src.exists() {
        anyhow::bail!(
            "phantom_cp.dll not found at {}. Build crates/cred-provider-win first, or pass --cp-dll.",
            dll_src.display()
        );
    }
    if !reg_src.exists() {
        anyhow::bail!(
            "phantom_cp.reg not found at {}. Build crates/cred-provider-win first, or pass --cp-reg.",
            reg_src.display()
        );
    }

    let dll_dst = std::path::PathBuf::from(r"C:\Windows\System32\phantom_cp.dll");
    println!("Installing Phantom Credential Provider...");
    std::fs::copy(&dll_src, &dll_dst)
        .with_context(|| format!("copy {} -> {}", dll_src.display(), dll_dst.display()))?;
    println!("  Copied {} -> {}", dll_src.display(), dll_dst.display());

    let status = std::process::Command::new("reg")
        .args(["import", reg_src.to_str().unwrap()])
        .status()
        .context("spawn reg import")?;
    if !status.success() {
        anyhow::bail!("reg import failed ({status}). Run as Administrator.");
    }
    println!("  Registered CLSID {CP_CLSID}");

    // Ensure C:\ProgramData\phantom exists so phantom-server can drop the
    // auth file there. Writable by LocalSystem (phantom-server service),
    // readable by LogonUI (SYSTEM).
    let _ = std::fs::create_dir_all(r"C:\ProgramData\phantom");

    println!("  Done.");
    println!("  Next:");
    println!("    1. Build phantom-server with --features sso");
    println!("    2. Run phantom-server with --sso-password-file <path-to-pw>");
    println!("    3. Next LogonUI invocation will pick up our CP");
    Ok(())
}

#[cfg(all(target_os = "windows", feature = "sso"))]
fn uninstall_credential_provider() -> Result<()> {
    println!("Uninstalling Phantom Credential Provider...");

    let _ = std::process::Command::new("reg")
        .args([
            "delete",
            &format!(
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{CP_CLSID}"
            ),
            "/f",
        ])
        .status();
    let _ = std::process::Command::new("reg")
        .args(["delete", &format!(r"HKCR\CLSID\{CP_CLSID}"), "/f"])
        .status();
    println!("  Unregistered CLSID");

    let dll = std::path::PathBuf::from(r"C:\Windows\System32\phantom_cp.dll");
    if dll.exists() {
        match std::fs::remove_file(&dll) {
            Ok(()) => println!("  Removed {}", dll.display()),
            Err(e) => println!(
                "  WARN: could not remove {}: {e} (LogonUI may have the DLL mapped; reboot then re-run)",
                dll.display()
            ),
        }
    }
    Ok(())
}

// ── Auto-start install/uninstall ────────────────────────────────────────────

fn install_autostart(display_mode: WindowsProvisioningMode) -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("get current exe path")?;
    #[allow(unused_variables)]
    let exe_str = exe.to_string_lossy();

    #[cfg(target_os = "windows")]
    return service_win::install_service(display_mode);

    #[cfg(not(target_os = "windows"))]
    let _ = display_mode;

    #[cfg(target_os = "linux")]
    {
        let service = format!(
            "[Unit]\nDescription=Phantom Remote Desktop Server\nAfter=graphical.target\n\n\
             [Service]\nType=simple\nExecStart={exe_str}\nRestart=always\nRestartSec=3\n\
             Environment=DISPLAY=:0\n\n[Install]\nWantedBy=default.target\n"
        );
        let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".config/systemd/user");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("phantom-server.service");
        std::fs::write(&path, &service)?;
        std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "phantom-server"])
            .status()?;
        println!("Installed: systemd user service");
        println!("  Status: systemctl --user status phantom-server");
        println!("  Remove: phantom-server --uninstall");
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        println!("Auto-start not yet supported on this OS. Run phantom-server manually.");
        Ok(())
    }
}

fn uninstall_autostart() -> Result<()> {
    #[cfg(target_os = "windows")]
    return service_win::uninstall_service();

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "phantom-server"])
            .status()?;
        let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".config/systemd/user/phantom-server.service");
        let _ = std::fs::remove_file(&path);
        println!("Removed: systemd user service");
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        println!("Auto-start not supported on this OS.");
        Ok(())
    }
}

// ── Agent mode (Windows only) ───────────────────────────────────────────────

/// Run as an agent process in the user's session.
/// Captures the screen via DXGI/scrap, sends frames to the service via IPC,
/// and receives input events to inject into the desktop.
#[cfg(target_os = "windows")]
fn run_agent_mode(
    ipc_session: Option<u32>,
    ipc_generation: Option<u64>,
    ipc_desktop: Option<&str>,
) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Agent has no console (spawned by the service). Set up tracing to write
    // to a log file in the system temp directory instead of stdout.
    let session_id = ipc_session.unwrap_or(0);
    let generation = ipc_generation.unwrap_or(0);
    let assigned_desktop = ipc_desktop
        .map(|value| {
            WindowsAgentDesktop::parse(value)
                .with_context(|| format!("invalid --ipc-desktop value: {value}"))
        })
        .transpose()?;
    let log_name = format!("phantom-agent-{session_id}-{generation}.log");
    let log_file = std::path::PathBuf::from(r"C:\Windows\Temp").join(&log_name);
    let file = std::fs::File::create(&log_file)
        .or_else(|_| std::fs::File::create(std::env::temp_dir().join(log_name)));
    if let Ok(file) = file {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("phantom=info".parse().unwrap()),
            )
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    } else {
        // Fallback: if we can't create the log file, init with default (stdout).
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("phantom=info".parse().unwrap()),
            )
            .init();
    }

    tracing::info!("Connecting to IPC pipe...");
    let ipc = match ipc_pipe::IpcClient::connect(ipc_session, ipc_generation) {
        Ok(c) => {
            tracing::info!("IPC connected");
            c
        }
        Err(e) => {
            tracing::error!("IPC connect FAILED: {e}");
            return Err(e);
        }
    };

    // Attach the agent's main thread to the current input desktop before
    // initializing anything that may create user32 windows/hooks. Once a
    // thread owns windows, SetThreadDesktop can no longer move it between
    // Winlogon and Default reliably.
    let window_station_ok = capture::gdi::switch_to_interactive_window_station();
    let input_desktop_ok = capture::gdi::switch_to_input_desktop();
    crate::service_win::svc_log(&format!(
        "agent bootstrap: winsta0={} input_desktop={} input={:?} thread={:?}",
        window_station_ok,
        input_desktop_ok,
        capture::gdi::current_input_desktop_name(),
        capture::gdi::current_thread_desktop_name()
    ));

    // Set up input injection
    let mut injector = match input_injector::InputInjector::new() {
        Ok(inj) => Some(inj),
        Err(e) => {
            tracing::warn!("Input injection unavailable: {e}");
            None
        }
    };

    // Graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .ok();
    }

    // Run agent capture+encode loop.
    // Like RustDesk/Sunshine: calls OpenInputDesktop+SetThreadDesktop before capture,
    // reinits DXGI on ACCESS_LOST (desktop switch: lock/unlock).
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_loop(&ipc, &mut injector, &shutdown, assigned_desktop)
    })) {
        Ok(result) => result,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                *s
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.as_str()
            } else {
                "unknown panic payload"
            };
            crate::service_win::svc_log(&format!("agent recovered from panic: {msg}"));
            anyhow::bail!("agent panic: {msg}");
        }
    }
}

/// Get the origin (left, top) of a display on the virtual desktop by matching resolution.
/// Returns (0, 0) if no matching display found (safe fallback for primary monitor).
#[cfg(target_os = "windows")]
fn get_display_origin(target_width: u32, target_height: u32, display_index: usize) -> (i32, i32) {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        // Collect all monitor rects
        let mut monitors: Vec<(i32, i32, u32, u32)> = Vec::new();
        unsafe extern "system" fn callback(
            _hmon: HMONITOR,
            _hdc: HDC,
            rect: *mut windows::Win32::Foundation::RECT,
            data: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::BOOL {
            if !rect.is_null() {
                let r = &*rect;
                let v = &mut *(data.0 as *mut Vec<(i32, i32, u32, u32)>);
                v.push((
                    r.left,
                    r.top,
                    (r.right - r.left) as u32,
                    (r.bottom - r.top) as u32,
                ));
            }
            true.into()
        }
        let data = windows::Win32::Foundation::LPARAM(&mut monitors as *mut _ as isize);
        let _ = EnumDisplayMonitors(None, None, Some(callback), data);

        tracing::info!(?monitors, "Virtual desktop monitors");

        // Find all monitors matching target resolution, pick by index
        let matching: Vec<_> = monitors
            .iter()
            .filter(|(_, _, w, h)| *w == target_width && *h == target_height)
            .collect();
        if let Some(&&(x, y, _, _)) =
            matching.get(display_index.min(matching.len().saturating_sub(1)))
        {
            tracing::info!(x, y, target_width, target_height, "Display origin found");
            (x, y)
        } else {
            tracing::warn!(
                "No monitor matching {}x{}, using (0,0)",
                target_width,
                target_height
            );
            (0, 0)
        }
    }
}

/// Find the VDD (Virtual Display Driver) device name (e.g. `\\.\DISPLAY10`).
/// Used to tell DXGI which output to capture — same approach as DCV/Parsec.
#[cfg(target_os = "windows")]
fn find_vdd_device_name() -> Option<String> {
    find_vdd_device_name_with_logging(true)
}

#[cfg(target_os = "windows")]
fn find_vdd_device_name_quiet() -> Option<String> {
    find_vdd_device_name_with_logging(false)
}

#[cfg(target_os = "windows")]
fn find_vdd_device_name_with_logging(log_missing: bool) -> Option<String> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mut device_idx = 0u32;
        let mut seen = Vec::new();
        loop {
            let mut dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(None, device_idx, &mut dd, 0).as_bool() {
                break;
            }
            let name = String::from_utf16_lossy(
                &dd.DeviceName[..dd
                    .DeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(dd.DeviceName.len())],
            );
            let desc = String::from_utf16_lossy(
                &dd.DeviceString[..dd
                    .DeviceString
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(dd.DeviceString.len())],
            );
            seen.push(format!(
                "[{device_idx}] name={name} desc={desc} state=0x{:X}",
                dd.StateFlags
            ));
            if desc == "Virtual Display Driver" {
                tracing::info!(name, desc, "Found VDD device");
                crate::service_win::svc_log(&format!("Found VDD device: {name} ({desc})"));
                return Some(name);
            }
            device_idx += 1;
        }
        if log_missing {
            if seen.is_empty() {
                crate::service_win::svc_log(
                    "VDD device not found; EnumDisplayDevicesW returned no display devices",
                );
            } else {
                crate::service_win::svc_log(&format!(
                    "VDD device not found; display devices: {}",
                    seen.join("; ")
                ));
            }
        }
        tracing::warn!("VDD device not found");
        None
    }
}

#[cfg(target_os = "windows")]
fn current_primary_display_device_name() -> Option<String> {
    current_primary_display_device_info().map(|(name, _)| name)
}

#[cfg(target_os = "windows")]
fn current_primary_display_device_info() -> Option<(String, String)> {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        let mut device_idx = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(None, device_idx, &mut dd, 0).as_bool() {
                return None;
            }
            device_idx += 1;
            if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) == 0
                || (dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) == 0
            {
                continue;
            }
            let name = String::from_utf16_lossy(
                &dd.DeviceName[..dd
                    .DeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(dd.DeviceName.len())],
            );
            if display_ccd::active_source_rect(&name).is_some() {
                let description = String::from_utf16_lossy(
                    &dd.DeviceString[..dd
                        .DeviceString
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(dd.DeviceString.len())],
                );
                return Some((name, description));
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn primary_display_is_basic() -> bool {
    current_primary_display_device_info()
        .is_none_or(|(_, description)| description.to_ascii_lowercase().contains("basic display"))
}

#[cfg(target_os = "windows")]
fn dcv_display_manager_present() -> bool {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|manager| manager.open_service("dcvserver", ServiceAccess::QUERY_STATUS))
        .and_then(|service| service.query_status())
        .is_ok_and(|status| status.current_state == ServiceState::Running)
}

#[cfg(target_os = "windows")]
fn external_display_manager_state() -> (bool, Option<String>) {
    let dcv_present = dcv_display_manager_present();
    let target = current_external_managed_display_device_name(dcv_present);
    (dcv_present || target.is_some(), target)
}

#[cfg(target_os = "windows")]
fn current_external_managed_display_device_name(dcv_present: bool) -> Option<String> {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        let mut device_idx = 0u32;
        let mut candidates = Vec::new();
        loop {
            let mut dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(None, device_idx, &mut dd, 0).as_bool() {
                break;
            }
            device_idx += 1;
            let description = String::from_utf16_lossy(
                &dd.DeviceString[..dd
                    .DeviceString
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(dd.DeviceString.len())],
            )
            .to_ascii_lowercase();
            let name = String::from_utf16_lossy(
                &dd.DeviceName[..dd
                    .DeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(dd.DeviceName.len())],
            );
            let Some((_, _, width, height)) = display_ccd::active_source_rect(&name) else {
                continue;
            };
            if description == "virtual display driver" || description.contains("basic display") {
                continue;
            }
            let explicitly_virtual =
                description.contains("indirect display") || description.contains("virtual display");
            candidates.push((explicitly_virtual, width as u64 * height as u64, name));
        }

        // NICE DCV can expose its managed console through a GPU-named path,
        // not only through an adapter whose description contains "virtual".
        candidates
            .into_iter()
            .filter(|(explicitly_virtual, _, _)| *explicitly_virtual || dcv_present)
            .max_by_key(|(explicitly_virtual, area, _)| (*explicitly_virtual, *area))
            .map(|(_, _, name)| name)
    }
}

#[cfg(target_os = "windows")]
fn current_display_resolution(device_name: &str) -> Option<(u32, u32)> {
    if let Some((_, _, width, height)) = display_ccd::active_source_rect(device_name) {
        return Some((width, height));
    }

    current_gdi_display_resolution(device_name)
}

#[cfg(target_os = "windows")]
fn current_gdi_display_resolution(device_name: &str) -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        let device_name_w: Vec<u16> = device_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let pcwstr = windows::core::PCWSTR(device_name_w.as_ptr());
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(pcwstr, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            Some((dm.dmPelsWidth, dm.dmPelsHeight))
        } else {
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn current_display_rect_for_device(device_name: &str) -> Option<(i32, i32, u32, u32)> {
    display_ccd::active_source_rect(device_name)
}

#[cfg(target_os = "windows")]
fn print_windows_display_diagnostics() -> Result<()> {
    use std::collections::BTreeSet;
    use windows::Win32::Graphics::Gdi::*;

    fn wide_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn built_in_modes() -> String {
        phantom_core::display_modes::VDD_MODE_BANK
            .iter()
            .map(|m| format!("{}x{}", m.width, m.height))
            .collect::<Vec<_>>()
            .join(", ")
    }

    unsafe fn enumerate_device_modes(device_name: &str) -> Vec<(u32, u32, u32)> {
        let device_name_w: Vec<u16> = device_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let pcwstr = windows::core::PCWSTR(device_name_w.as_ptr());
        let mut modes = BTreeSet::new();
        let mut mode_idx = 0u32;
        loop {
            let mut dm = DEVMODEW::default();
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if !EnumDisplaySettingsW(pcwstr, ENUM_DISPLAY_SETTINGS_MODE(mode_idx), &mut dm)
                .as_bool()
            {
                break;
            }
            modes.insert((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency));
            mode_idx += 1;
        }
        modes.into_iter().collect()
    }

    println!("Phantom Windows display diagnostics");
    println!(
        "  built-in VDD mode bank ({}): {}",
        phantom_core::display_modes::VDD_MODE_BANK.len(),
        built_in_modes()
    );

    println!("\nGDI display devices:");
    unsafe {
        let mut device_idx = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(None, device_idx, &mut dd, 0).as_bool() {
                break;
            }
            let name = wide_to_string(&dd.DeviceName);
            let desc = wide_to_string(&dd.DeviceString);
            println!(
                "  [{device_idx}] name={name} desc={desc} state=0x{:X}",
                dd.StateFlags
            );
            device_idx += 1;
        }
    }

    println!("\nCCD active topology:");
    match display_ccd::active_config_summary() {
        Ok(lines) if lines.is_empty() => println!("  <none>"),
        Ok(lines) => {
            for line in lines {
                println!("  {line}");
            }
        }
        Err(e) => println!("  error: {e:#}"),
    }

    println!("\nManaged VDD:");
    match find_vdd_device_name() {
        Some(vdd) => {
            println!("  device: {vdd}");
            println!("  primary: {}", display_ccd::is_vdd_primary(&vdd));
            match current_display_rect_for_device(&vdd) {
                Some((x, y, width, height)) => {
                    println!("  rect: {width}x{height} at ({x},{y})");
                }
                None => println!("  rect: <unavailable>"),
            }
            let modes = unsafe { enumerate_device_modes(&vdd) };
            if modes.is_empty() {
                println!("  advertised modes: <none from EnumDisplaySettingsW>");
            } else {
                println!("  advertised modes ({}):", modes.len());
                for (width, height, refresh) in modes {
                    println!("    {width}x{height}@{refresh}");
                }
            }
        }
        None => println!("  <not found>"),
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn provision_windows_vdd_display() -> Result<()> {
    let _ = capture::gdi::switch_to_interactive_window_station();
    if !capture::gdi::switch_to_input_desktop() {
        anyhow::bail!(
            "cannot attach to the input desktop; run provisioning from an interactive session"
        );
    }

    let vdd = find_vdd_device_name().context("Virtual Display Driver not found")?;
    provision_windows_vdd_target(&vdd)
}

#[cfg(target_os = "windows")]
fn provision_windows_vdd_target(vdd: &str) -> Result<()> {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;

    if display_ccd::active_path_count().ok() == Some(1)
        && display_ccd::active_source_rect(vdd) == Some((0, 0, WIDTH, HEIGHT))
    {
        crate::service_win::svc_log(&format!(
            "managed display already stable: {vdd} {WIDTH}x{HEIGHT}, sole active path"
        ));
        return Ok(());
    }

    let original = display_ccd::provision_single_display(vdd)?;
    let resolution = display_ccd::set_source_resolution(vdd, WIDTH, HEIGHT);
    if let Err(e) = &resolution {
        crate::service_win::svc_log(&format!(
            "CCD provisioning resolution failed for {vdd}: {e:#}"
        ));
    }

    let observed = display_ccd::active_source_rect(vdd);
    let active_count = display_ccd::active_path_count().ok();
    if resolution.is_err() || active_count != Some(1) || observed != Some((0, 0, WIDTH, HEIGHT)) {
        let restore = display_ccd::restore_topology(&original);
        anyhow::bail!(
            "managed VDD provisioning failed: resolution_ok={} active_paths={active_count:?} observed={observed:?}; rollback={restore:?}",
            resolution.is_ok()
        );
    }

    crate::service_win::svc_log(&format!(
        "Managed VDD provisioned: {vdd} {WIDTH}x{HEIGHT}, sole active display"
    ));
    Ok(())
}

#[cfg(target_os = "windows")]
fn nudge_windows_capture_target(
    target_device: Option<&str>,
    target_rect: Option<(i32, i32, u32, u32)>,
) {
    if let Some((x, y, width, height)) =
        target_rect.or_else(|| target_device.and_then(current_display_rect_for_device))
    {
        input_injector::windows_nudge_cursor_in_rect(x, y, width, height);
    } else {
        input_injector::windows_nudge_cursor_for_capture();
    }
}

#[cfg(target_os = "windows")]
fn change_display_resolution_for_device(device_name: &str, width: u32, height: u32) -> bool {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        let device_name_w: Vec<u16> = device_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let pcwstr = windows::core::PCWSTR(device_name_w.as_ptr());
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

        if !EnumDisplaySettingsW(pcwstr, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            tracing::warn!("EnumDisplaySettingsW failed for {device_name}");
            return false;
        }

        dm.dmPelsWidth = width;
        dm.dmPelsHeight = height;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;

        let attempts = [
            ("transient", CDS_TYPE(0), false),
            ("registry_noreset", CDS_UPDATEREGISTRY | CDS_NORESET, true),
        ];

        for (label, flags, needs_global_apply) in attempts {
            let result = ChangeDisplaySettingsExW(pcwstr, Some(&dm), None, flags, None);
            crate::service_win::svc_log(&format!(
                "display manager: ChangeDisplaySettingsExW {label} {device_name} {width}x{height} -> {result:?}"
            ));
            if result != DISP_CHANGE_SUCCESSFUL {
                continue;
            }

            if needs_global_apply {
                let reset = ChangeDisplaySettingsExW(None, None, None, CDS_TYPE(0), None);
                crate::service_win::svc_log(&format!(
                    "display manager: ChangeDisplaySettingsExW global apply after {label} -> {reset:?}"
                ));
                if reset != DISP_CHANGE_SUCCESSFUL {
                    continue;
                }
            }

            let mut observed = None;
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                // Verify the legacy/GDI view specifically. CCD may already
                // report the requested source mode while DXGI still exposes
                // the old duplication surface, which is the inconsistency
                // this fallback is meant to repair.
                observed = current_gdi_display_resolution(device_name);
                if observed == Some((width, height)) {
                    tracing::info!(
                        width,
                        height,
                        device = device_name,
                        "Display resolution changed"
                    );
                    return true;
                }
            }

            match observed {
                Some((observed_w, observed_h)) => crate::service_win::svc_log(&format!(
                    "display manager: ChangeDisplaySettingsExW {label} reported success but observed {observed_w}x{observed_h} after readiness timeout"
                )),
                None => crate::service_win::svc_log(&format!(
                    "display manager: ChangeDisplaySettingsExW {label} reported success but display rect remained unavailable"
                )),
            }
        }

        tracing::warn!(
            width,
            height,
            device = device_name,
            "ChangeDisplaySettingsExW failed"
        );
        false
    }
}

/// Agent capture+encode loop following RustDesk/Sunshine pattern:
/// - Calls OpenInputDesktop + SetThreadDesktop before capture (follows desktop switches)
/// - On DXGI error: reinit pipeline (don't crash)
/// - Survives lock/unlock transitions
#[cfg(target_os = "windows")]
fn create_windows_agent_encoder(
    width: u32,
    height: u32,
    fps: f32,
    bitrate_kbps: u32,
    prefer_nvenc: bool,
) -> Option<(Box<dyn phantom_core::encode::FrameEncoder>, &'static str)> {
    if prefer_nvenc {
        match create_encoder("nvenc", width, height, fps, bitrate_kbps, VideoCodec::H264) {
            Ok(mut enc) => {
                enc.force_keyframe();
                return Some((enc, "NVENC"));
            }
            Err(e) => {
                crate::service_win::svc_log(&format!(
                    "Windows agent: NVENC CPU-frame encoder unavailable; using OpenH264: {e:#}"
                ));
            }
        }
    }
    match encode::h264::OpenH264Encoder::new(width, height, fps, bitrate_kbps) {
        Ok(mut enc) => {
            enc.force_keyframe();
            Some((Box::new(enc), "OpenH264"))
        }
        Err(e) => {
            tracing::warn!("OpenH264 init failed: {e}");
            None
        }
    }
}

#[cfg(target_os = "windows")]
#[expect(
    clippy::too_many_arguments,
    reason = "Fallback borrows caller-owned capture state; keep the individual mutable fields explicit"
)]
fn activate_gdi_fallback(
    cpu_encoder: &mut Option<Box<dyn phantom_core::encode::FrameEncoder>>,
    gdi_capture: &mut Option<capture::gdi::GdiCapture>,
    width: &mut u32,
    height: &mut u32,
    display_x: &mut i32,
    display_y: &mut i32,
    capture_mode: &mut &'static str,
    target_device: Option<&str>,
    prefer_nvenc: bool,
) -> bool {
    if let Some(device) = target_device {
        let target_rect = current_display_rect_for_device(device);
        if let Some((origin_x, origin_y, rect_w, rect_h)) = target_rect {
            crate::service_win::svc_log(&format!(
                "GDI fallback: targeting display DC {device} rect=({}, {}) {}x{}",
                origin_x, origin_y, rect_w, rect_h
            ));
            return activate_gdi_fallback_region(
                cpu_encoder,
                gdi_capture,
                width,
                height,
                display_x,
                display_y,
                capture_mode,
                prefer_nvenc,
                capture::gdi::GdiCaptureRegion::DisplayDevice {
                    device_name: device.to_string(),
                    origin_x,
                    origin_y,
                    width: rect_w,
                    height: rect_h,
                },
            );
        }
        crate::service_win::svc_log(&format!(
            "GDI fallback: target {device} unavailable; refusing to switch to another display"
        ));
        return false;
    }

    crate::service_win::svc_log(
        "GDI fallback: no managed display target; using primary monitor as the target",
    );
    activate_gdi_primary_fallback(
        cpu_encoder,
        gdi_capture,
        width,
        height,
        display_x,
        display_y,
        capture_mode,
        prefer_nvenc,
    )
}

#[cfg(target_os = "windows")]
#[expect(
    clippy::too_many_arguments,
    reason = "Fallback borrows caller-owned capture state; keep the individual mutable fields explicit"
)]
fn activate_gdi_primary_fallback(
    cpu_encoder: &mut Option<Box<dyn phantom_core::encode::FrameEncoder>>,
    gdi_capture: &mut Option<capture::gdi::GdiCapture>,
    width: &mut u32,
    height: &mut u32,
    display_x: &mut i32,
    display_y: &mut i32,
    capture_mode: &mut &'static str,
    prefer_nvenc: bool,
) -> bool {
    activate_gdi_fallback_region(
        cpu_encoder,
        gdi_capture,
        width,
        height,
        display_x,
        display_y,
        capture_mode,
        prefer_nvenc,
        capture::gdi::GdiCaptureRegion::PrimaryMonitor,
    )
}

#[cfg(target_os = "windows")]
#[expect(
    clippy::too_many_arguments,
    reason = "Fallback borrows caller-owned capture state; keep the individual mutable fields explicit"
)]
fn activate_gdi_fallback_region(
    cpu_encoder: &mut Option<Box<dyn phantom_core::encode::FrameEncoder>>,
    gdi_capture: &mut Option<capture::gdi::GdiCapture>,
    width: &mut u32,
    height: &mut u32,
    display_x: &mut i32,
    display_y: &mut i32,
    capture_mode: &mut &'static str,
    prefer_nvenc: bool,
    region: capture::gdi::GdiCaptureRegion,
) -> bool {
    let (fps, bitrate_kbps) = if prefer_nvenc {
        // Logged-in desktop fallback is user-interactive. Keep it at the same
        // target as Scrap/DXGI so dragging windows does not feel capped at 15fps.
        (30.0, 5000)
    } else {
        // Login screen is mostly static and should avoid exercising NVENC/VDD.
        (15.0, 2000)
    };
    let gdi_result = match &region {
        capture::gdi::GdiCaptureRegion::PrimaryMonitor => {
            capture::gdi::GdiCapture::new_primary_monitor()
        }
        capture::gdi::GdiCaptureRegion::VirtualScreen => capture::gdi::GdiCapture::new(),
        capture::gdi::GdiCaptureRegion::Rect {
            origin_x,
            origin_y,
            width,
            height,
        } => capture::gdi::GdiCapture::new_for_rect(*origin_x, *origin_y, *width, *height),
        capture::gdi::GdiCaptureRegion::DisplayDevice {
            device_name,
            origin_x,
            origin_y,
            width,
            height,
        } => capture::gdi::GdiCapture::new_for_display_device(
            device_name.clone(),
            *origin_x,
            *origin_y,
            *width,
            *height,
        ),
    };
    match gdi_result {
        Ok(gdi) => {
            let (w, h) = phantom_core::capture::FrameCapture::resolution(&gdi);
            let (origin_x, origin_y) = gdi.origin();
            *width = w;
            *height = h;
            *display_x = origin_x;
            *display_y = origin_y;
            // GDI is the safety/bootstrap path for lock/login/transition
            // states. Keep it independent from NVENC because D3D/NVENC can be
            // exactly the subsystem that is wedged during those transitions.
            if let Some((enc, encoder_name)) =
                create_windows_agent_encoder(w, h, fps, bitrate_kbps, false)
            {
                crate::service_win::svc_log(&format!(
                    "Tier 3: GDI+{} {}x{} origin=({}, {}) region={:?} fps={:.0} bitrate={}kbps",
                    encoder_name, *width, *height, origin_x, origin_y, region, fps, bitrate_kbps
                ));
                *cpu_encoder = Some(enc);
                *gdi_capture = Some(gdi);
                *capture_mode = if encoder_name == "NVENC" {
                    "gdi_nvenc"
                } else {
                    "gdi_h264"
                };
                true
            } else {
                false
            }
        }
        Err(e) => {
            tracing::warn!("GDI fallback init failed: {e}");
            false
        }
    }
}

#[cfg(target_os = "windows")]
#[expect(
    clippy::too_many_arguments,
    reason = "Fallback borrows caller-owned capture state; keep the individual mutable fields explicit"
)]
fn activate_cpu_fallback(
    reason: &str,
    cpu_encoder: &mut Option<Box<dyn phantom_core::encode::FrameEncoder>>,
    scrap_capture: &mut Option<capture::scrap::ScrapCapture>,
    gdi_capture: &mut Option<capture::gdi::GdiCapture>,
    width: &mut u32,
    height: &mut u32,
    display_x: &mut i32,
    display_y: &mut i32,
    capture_mode: &mut &'static str,
    scrap_waiting_for_frame_since: &mut Option<std::time::Instant>,
    target_device: Option<&str>,
    target_rect: Option<(i32, i32, u32, u32)>,
    allow_gdi: bool,
) {
    use phantom_core::capture::FrameCapture;

    crate::service_win::svc_log(reason);

    let displays = capture::scrap::ScrapCapture::list_displays().unwrap_or_default();
    let best_idx = if let Some(device) = target_device {
        match capture::scrap::ScrapCapture::windows_display_index_for_device_name(device) {
            Ok(Some(index)) => Some(index),
            Ok(None) => {
                crate::service_win::svc_log(&format!(
                    "ScrapCapture fallback: target {device} not found in DXGI display list; skipping Scrap"
                ));
                None
            }
            Err(e) => {
                crate::service_win::svc_log(&format!(
                    "ScrapCapture fallback: failed to enumerate target {device}: {e:#}; skipping Scrap"
                ));
                None
            }
        }
    } else {
        displays
            .iter()
            .find(|d| d.is_primary)
            .or_else(|| displays.first())
            .map(|d| d.index)
    };
    tracing::info!(
        ?best_idx,
        target_device = ?target_device,
        displays = ?displays.iter().map(|d| format!("{}:{}x{}", d.index, d.width, d.height)).collect::<Vec<_>>(),
        "ScrapCapture fallback display selection"
    );

    let Some(best_idx) = best_idx else {
        if !allow_gdi {
            crate::service_win::svc_log(
                "ScrapCapture fallback had no matching display; refusing GDI on managed Default desktop",
            );
            return;
        }
        if activate_gdi_fallback(
            cpu_encoder,
            gdi_capture,
            width,
            height,
            display_x,
            display_y,
            capture_mode,
            target_device,
            true,
        ) {
            tracing::info!(width, height, "Tier 3: same-target GDI fallback");
            *scrap_waiting_for_frame_since = None;
        }
        return;
    };

    match capture::scrap::ScrapCapture::with_display(best_idx) {
        Ok(scrap) => {
            let (w, h) = scrap.resolution();
            let (dx, dy) = if let Some(device) = target_device {
                match target_rect.or_else(|| current_display_rect_for_device(device)) {
                    Some((x, y, rect_w, rect_h)) if rect_w == w && rect_h == h => (x, y),
                    Some((x, y, rect_w, rect_h)) => {
                        crate::service_win::svc_log(&format!(
                            "ScrapCapture fallback: target {device} rect=({}, {}) {}x{} but Scrap index {} is {}x{}; refusing to switch target",
                            x, y, rect_w, rect_h, best_idx, w, h
                        ));
                        if !allow_gdi {
                            crate::service_win::svc_log(
                                "ScrapCapture fallback target mismatch; refusing GDI on managed Default desktop",
                            );
                            return;
                        }
                        if activate_gdi_fallback(
                            cpu_encoder,
                            gdi_capture,
                            width,
                            height,
                            display_x,
                            display_y,
                            capture_mode,
                            target_device,
                            true,
                        ) {
                            *scrap_waiting_for_frame_since = None;
                        }
                        return;
                    }
                    None => {
                        crate::service_win::svc_log(&format!(
                            "ScrapCapture fallback: target {device} has no active rect; refusing to switch target"
                        ));
                        if !allow_gdi {
                            crate::service_win::svc_log(
                                "ScrapCapture fallback target inactive; refusing GDI on managed Default desktop",
                            );
                            return;
                        }
                        if activate_gdi_fallback(
                            cpu_encoder,
                            gdi_capture,
                            width,
                            height,
                            display_x,
                            display_y,
                            capture_mode,
                            target_device,
                            true,
                        ) {
                            *scrap_waiting_for_frame_since = None;
                        }
                        return;
                    }
                }
            } else {
                get_display_origin(w, h, scrap.display_index())
            };
            *width = w;
            *height = h;
            *display_x = dx;
            *display_y = dy;
            if let Some((enc, encoder_name)) =
                create_windows_agent_encoder(*width, *height, 30.0, 5000, true)
            {
                crate::service_win::svc_log(&format!(
                    "Tier 2: ScrapCapture+{} {}x{} origin=({}, {}) target={:?} (CPU capture path)",
                    encoder_name, *width, *height, dx, dy, target_device
                ));
                *scrap_capture = Some(scrap);
                *cpu_encoder = Some(enc);
                *scrap_waiting_for_frame_since = Some(std::time::Instant::now());
                *capture_mode = if encoder_name == "NVENC" {
                    "scrap_nvenc"
                } else {
                    "scrap_h264"
                };
                nudge_windows_capture_target(target_device, target_rect);
            }
        }
        Err(e) => {
            tracing::debug!("ScrapCapture unavailable: {e}");
            if !allow_gdi {
                crate::service_win::svc_log(&format!(
                    "ScrapCapture unavailable and GDI is disabled on managed Default desktop: {e:#}"
                ));
                return;
            }
            if activate_gdi_fallback(
                cpu_encoder,
                gdi_capture,
                width,
                height,
                display_x,
                display_y,
                capture_mode,
                target_device,
                true,
            ) {
                tracing::info!(width, height, "Tier 3: same-target GDI fallback");
                *scrap_waiting_for_frame_since = None;
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn coalesce_mouse_moves(
    inputs: Vec<phantom_core::input::InputEvent>,
) -> Vec<phantom_core::input::InputEvent> {
    let mut out = Vec::with_capacity(inputs.len());
    let mut pending_move: Option<phantom_core::input::InputEvent> = None;
    let mut mouse_buttons_down = 0u8;

    for event in inputs {
        match event {
            phantom_core::input::InputEvent::MouseMove { .. } if mouse_buttons_down == 0 => {
                pending_move = Some(event);
            }
            phantom_core::input::InputEvent::MouseMove { .. } => {
                if let Some(mv) = pending_move.take() {
                    out.push(mv);
                }
                out.push(event);
            }
            phantom_core::input::InputEvent::MouseButton { pressed, .. } => {
                if let Some(mv) = pending_move.take() {
                    out.push(mv);
                }
                if pressed {
                    mouse_buttons_down = mouse_buttons_down.saturating_add(1);
                } else {
                    mouse_buttons_down = mouse_buttons_down.saturating_sub(1);
                }
                out.push(event);
            }
            event => {
                if let Some(mv) = pending_move.take() {
                    out.push(mv);
                }
                out.push(event);
            }
        }
    }

    if let Some(mv) = pending_move {
        out.push(mv);
    }

    out
}

#[cfg(target_os = "windows")]
struct DesktopState {
    on_default: bool,
    transition_unavailable: bool,
    changed_after_initial: bool,
    desktop_name: Option<String>,
}

#[cfg(target_os = "windows")]
impl DesktopState {
    fn phase(&self) -> WindowsDesktopPhase {
        if self.transition_unavailable {
            WindowsDesktopPhase::Transition
        } else if self.on_default {
            WindowsDesktopPhase::Default
        } else {
            WindowsDesktopPhase::Winlogon
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsDisplayManager {
    vdd_device: Option<String>,
    capture_target_device: Option<String>,
    capture_target_external: bool,
    vdd_rect: Option<(i32, i32, u32, u32)>,
    vdd_primary_active: bool,
    provisioning_mode: WindowsProvisioningMode,
    external_display_manager: bool,
    explicit_external_target: Option<String>,
    last_external_owner_probe: Instant,
    provisioning_ready: bool,
    provisioning_not_before: Instant,
    last_provisioning_attempt: Instant,
    last_provisioning_wait_log: Instant,
    last_desktop_name: Option<String>,
    desktop_sample_seen: bool,
    last_desktop_transition_log: Instant,
    last_display_discovery_attempt: Instant,
    last_display_discovery_log: Instant,
}

#[cfg(target_os = "windows")]
impl WindowsDisplayManager {
    fn new() -> Self {
        // Attach to input desktop BEFORE calling SetDisplayConfig. CCD API
        // returns ERROR_ACCESS_DENIED if the calling thread isn't attached to
        // an interactive desktop. Same reason capture calls need this.
        let window_station_ok = capture::gdi::switch_to_interactive_window_station();
        let input_desktop_ok = capture::gdi::switch_to_input_desktop();
        crate::service_win::svc_log(&format!(
            "display manager bootstrap: winsta0={} input_desktop={} input={:?} thread={:?}",
            window_station_ok,
            input_desktop_ok,
            capture::gdi::current_input_desktop_name(),
            capture::gdi::current_thread_desktop_name()
        ));

        // Discover VDD for ownership policy. Dedicated managed mode may
        // provision it before capture starts; externally managed and preserve
        // modes never mutate topology.
        let vdd_device = find_vdd_device_name();
        crate::service_win::svc_log(&format!("agent: vdd_device = {:?}", vdd_device));

        // Log displays before any topology change so we can diagnose which
        // adapter Tier 2 ends up targeting.
        if let Ok(displays) = capture::scrap::ScrapCapture::list_displays() {
            for d in &displays {
                crate::service_win::svc_log(&format!(
                    "agent: display[{}] {}x{} primary={}",
                    d.index, d.width, d.height, d.is_primary
                ));
            }
        }

        let provisioning_mode = crate::service_win::display_provisioning_mode();
        let (external_display_manager, explicit_external_target) = external_display_manager_state();
        let capture_target_device = current_primary_display_device_name();
        let manager = Self {
            vdd_rect: vdd_device
                .as_deref()
                .and_then(display_ccd::active_source_rect),
            vdd_primary_active: vdd_device
                .as_deref()
                .is_some_and(display_ccd::is_vdd_primary),
            provisioning_mode,
            external_display_manager,
            explicit_external_target,
            last_external_owner_probe: Instant::now(),
            provisioning_ready: provisioning_mode == WindowsProvisioningMode::PreserveConsole,
            provisioning_not_before: Instant::now()
                + if provisioning_mode == WindowsProvisioningMode::Auto && external_display_manager
                {
                    Duration::from_secs(2)
                } else {
                    Duration::ZERO
                },
            last_provisioning_attempt: instant_ago(Duration::from_secs(10)),
            last_provisioning_wait_log: instant_ago(Duration::from_secs(10)),
            vdd_device,
            capture_target_device,
            capture_target_external: false,
            last_desktop_name: None,
            desktop_sample_seen: false,
            last_desktop_transition_log: instant_ago(Duration::from_secs(10)),
            last_display_discovery_attempt: instant_ago(Duration::from_secs(10)),
            last_display_discovery_log: instant_ago(Duration::from_secs(10)),
        };

        manager
    }

    fn refresh_external_display_ownership(&mut self) {
        if self.last_external_owner_probe.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_external_owner_probe = Instant::now();

        let (detected_manager, detected_target) = external_display_manager_state();
        let newly_external = detected_manager && !self.external_display_manager;
        let explicit_target_changed = detected_target.is_some()
            && detected_target.as_deref() != self.explicit_external_target.as_deref();
        self.external_display_manager |= detected_manager;
        self.explicit_external_target = detected_target;

        if newly_external {
            self.provisioning_ready = false;
            self.capture_target_external = false;
            self.provisioning_not_before = Instant::now() + Duration::from_secs(2);
            self.last_provisioning_attempt = instant_ago(Duration::from_secs(10));
            crate::service_win::svc_log(
                "display manager: detected an external topology owner; holding capture until its target settles",
            );
        } else if explicit_target_changed && self.capture_target_external {
            self.provisioning_ready = false;
            self.capture_target_external = false;
            self.provisioning_not_before = Instant::now();
            self.last_provisioning_attempt = instant_ago(Duration::from_secs(10));
            crate::service_win::svc_log(&format!(
                "display manager: external capture target changed to {:?}; reconciling without mutating topology",
                self.explicit_external_target
            ));
        }
    }

    fn refresh_capture_target(&mut self) {
        self.capture_target_device = current_primary_display_device_name();
        self.vdd_rect = self
            .vdd_device
            .as_deref()
            .and_then(display_ccd::active_source_rect);
        self.vdd_primary_active = self
            .vdd_device
            .as_deref()
            .is_some_and(display_ccd::is_vdd_primary);
    }

    /// Establish the display target before capture starts. Default desktop
    /// fails closed; a managed Winlogon agent retries briefly, then preserves
    /// the current console target so the login screen remains reachable.
    fn ensure_provisioning_ready(&mut self, desktop_state: &DesktopState) -> bool {
        if desktop_state.transition_unavailable {
            return false;
        }
        self.refresh_external_display_ownership();
        if self.provisioning_ready {
            if self.ready_topology_still_valid(desktop_state) {
                return true;
            }
            crate::service_win::svc_log(
                "display manager: active topology drifted after readiness; holding capture and reconciling again",
            );
            self.provisioning_ready = false;
            self.capture_target_external = false;
            self.refresh_capture_target();
            self.last_provisioning_attempt = instant_ago(Duration::from_secs(10));
        }

        let input_desktop = capture::gdi::current_input_desktop_name();
        let expected_desktop = if desktop_state.on_default {
            "Default"
        } else {
            "Winlogon"
        };
        let input_ready = input_desktop
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected_desktop))
            && capture::gdi::switch_to_input_desktop();
        if !input_ready {
            if self.last_provisioning_wait_log.elapsed() >= Duration::from_secs(1) {
                crate::service_win::svc_log(&format!(
                    "display manager: waiting for {expected_desktop} input desktop before provisioning; input={input_desktop:?}"
                ));
                self.last_provisioning_wait_log = Instant::now();
            }
            return false;
        }

        if !desktop_state.on_default {
            // Winlogon owns the secure desktop's display surface. A fresh
            // post-sign-out console can start at 800x600 even when the user's
            // managed VDD was 1920x1080. Reconfiguring CCD after LogonUI has
            // initialized changes the path size without reliably resizing the
            // secure-desktop surface, leaving an 800x600 image in one corner.
            // Capture Windows' active console exactly as exposed and defer
            // Phantom-owned topology changes until Default.
            self.refresh_capture_target();
            self.provisioning_ready = true;
            crate::service_win::svc_log(if self.external_display_manager {
                "display manager: preserving externally managed Winlogon topology"
            } else {
                "display manager: preserving Windows-owned Winlogon topology"
            });
            return true;
        }

        if Instant::now() < self.provisioning_not_before {
            if self.last_provisioning_wait_log.elapsed() >= Duration::from_secs(1) {
                crate::service_win::svc_log(
                    "display manager: waiting up to 2s for external managed display readiness",
                );
                self.last_provisioning_wait_log = Instant::now();
            }
            return false;
        }
        if self.last_provisioning_attempt.elapsed() < Duration::from_millis(500) {
            return false;
        }
        self.last_provisioning_attempt = Instant::now();

        let explicit_external_display_target = self.explicit_external_target.clone();
        // Once an external manager is known to own topology, never race it by
        // provisioning Phantom's VDD. Some managers expose a GPU-named path
        // that our virtual-display heuristic cannot identify during startup;
        // after the readiness grace, capturing the active CCD primary is the
        // safe fallback because it does not change topology and will be
        // invalidated if the manager later switches paths.
        let external_display_target = explicit_external_display_target.clone().or_else(|| {
            self.external_display_manager
                .then(current_primary_display_device_name)
                .flatten()
        });
        let active_paths = display_ccd::active_path_count().ok();
        let vdd_active = self
            .vdd_device
            .as_deref()
            .is_some_and(|vdd| display_ccd::active_source_rect(vdd).is_some());
        let topology = classify_topology(self.vdd_device.is_some(), vdd_active, active_paths);
        // A running external manager owns topology even while its display path
        // is still coming up. Treating a temporarily missing target as no owner
        // lets Phantom provision its VDD and races DCV into a duplicate desktop.
        let external_owner_preferred = self.external_display_manager;
        let provisioning = decide_display_provisioning(
            self.provisioning_mode,
            topology,
            primary_display_is_basic(),
            external_owner_preferred,
            external_display_target.is_some(),
        );
        crate::service_win::svc_log(&format!(
            "display manager: provisioning policy={:?} topology={topology:?} external_manager={} external_owner_preferred={external_owner_preferred} explicit_external_target={explicit_external_display_target:?} capture_target={external_display_target:?} decision={provisioning:?} primary={:?}",
            self.provisioning_mode,
            self.external_display_manager,
            current_primary_display_device_info()
        ));

        let result = match provisioning {
            WindowsProvisioningDecision::ProvisionVdd => self
                .vdd_device
                .as_deref()
                .context("VDD provisioning requested but Phantom VDD is unavailable")
                .and_then(provision_windows_vdd_target),
            WindowsProvisioningDecision::PreserveExisting => Ok(()),
            WindowsProvisioningDecision::AdoptExternal => external_display_target
                .as_deref()
                .context("external display target disappeared before adoption")
                .map(|_| ()),
            WindowsProvisioningDecision::Defer => return false,
        };

        match result {
            Ok(()) => {
                self.refresh_capture_target();
                self.capture_target_external = false;
                if provisioning == WindowsProvisioningDecision::AdoptExternal {
                    self.capture_target_device = external_display_target.clone();
                    self.capture_target_external = true;
                }
                self.provisioning_ready = true;
                crate::service_win::svc_log(match (
                    self.provisioning_mode,
                    provisioning,
                ) {
                    (WindowsProvisioningMode::Auto, WindowsProvisioningDecision::ProvisionVdd) => {
                        "display manager: auto display policy ready with single Phantom VDD"
                    }
                    (WindowsProvisioningMode::Auto, WindowsProvisioningDecision::AdoptExternal) => {
                        "display manager: auto display policy ready using externally managed target without mutating topology"
                    }
                    (WindowsProvisioningMode::Auto, _) => {
                        "display manager: auto display policy ready by adopting the sole existing display"
                    }
                    _ => "display manager: managed single-display provisioning ready",
                });
                true
            }
            Err(error) => {
                crate::service_win::svc_log(&format!(
                    "display manager: provisioning failed; holding capture and retrying: {error:#}"
                ));
                false
            }
        }
    }

    fn policy_for(&self, desktop_state: &DesktopState) -> WindowsDisplayPolicy {
        select_windows_display_policy(desktop_state.phase(), self.managed_vdd_target().is_some())
    }

    fn ready_topology_still_valid(&self, desktop_state: &DesktopState) -> bool {
        if !desktop_state.on_default
            || self.provisioning_mode == WindowsProvisioningMode::PreserveConsole
        {
            return true;
        }
        let Some(target) = self.capture_target_device() else {
            return false;
        };
        let target_active = display_ccd::active_source_rect(target).is_some();
        if self.capture_target_external {
            external_capture_target_still_valid(
                target,
                target_active,
                self.explicit_external_target.as_deref(),
            )
        } else {
            display_ccd::active_path_count().ok() == Some(1) && target_active
        }
    }

    fn invalidate_if_ready_topology_drifted(&mut self, desktop_state: &DesktopState) -> bool {
        if self.ready_topology_still_valid(desktop_state) {
            return false;
        }
        self.provisioning_ready = false;
        self.capture_target_external = false;
        self.refresh_capture_target();
        self.last_provisioning_attempt = instant_ago(Duration::from_secs(10));
        crate::service_win::svc_log(
            "display manager: topology changed during DXGI initialization; discarding candidate before first frame",
        );
        true
    }

    fn topology_kind(&self) -> WindowsTopologyKind {
        let active_paths = match display_ccd::active_path_count() {
            Ok(count) => Some(count),
            Err(e) => {
                crate::service_win::svc_log(&format!(
                    "display manager: failed to read active topology: {e:#}"
                ));
                None
            }
        };

        let vdd_active = self
            .vdd_device
            .as_deref()
            .is_some_and(|device| display_ccd::active_source_rect(device).is_some());

        classify_topology(self.vdd_device.is_some(), vdd_active, active_paths)
    }

    fn snapshot_summary(&self, desktop_state: &DesktopState) -> String {
        let active_paths = display_ccd::active_path_count()
            .map(|count| count.to_string())
            .unwrap_or_else(|e| format!("unknown({e:#})"));
        let vdd_active = self
            .vdd_device
            .as_deref()
            .is_some_and(|device| display_ccd::active_source_rect(device).is_some());
        let capture_target_rect = self.capture_target_rect();
        format!(
            "policy={:?} phase={:?} topology={:?} active_paths={} capture_target={:?} capture_target_rect={:?} vdd={:?} vdd_active={} vdd_primary={} vdd_rect={:?}",
            self.policy_for(desktop_state),
            desktop_state.phase(),
            self.topology_kind(),
            active_paths,
            self.capture_target_device,
            capture_target_rect,
            self.vdd_device,
            vdd_active,
            self.vdd_primary_active,
            self.vdd_rect
        )
    }

    fn log_snapshot(&self, reason: &str, desktop_state: &DesktopState) {
        crate::service_win::svc_log(&format!(
            "display manager: {reason}: {}",
            self.snapshot_summary(desktop_state)
        ));
        if let Ok(lines) = display_ccd::active_config_summary() {
            for line in lines {
                crate::service_win::svc_log(&format!("display manager: {reason}: {line}"));
            }
        }
    }

    fn capture_target_device(&self) -> Option<&str> {
        self.capture_target_device.as_deref()
    }

    fn capture_target_rect(&self) -> Option<(i32, i32, u32, u32)> {
        self.capture_target_device()
            .and_then(current_display_rect_for_device)
    }

    fn managed_vdd_target(&self) -> Option<&str> {
        if self.capture_target_external
            || self.topology_kind() != WindowsTopologyKind::SingleManagedVdd
        {
            return None;
        }
        match (self.vdd_device.as_deref(), self.capture_target_device()) {
            (Some(vdd), Some(target)) if vdd.eq_ignore_ascii_case(target) => Some(vdd),
            _ => None,
        }
    }

    fn display_device_count() -> usize {
        use windows::Win32::Graphics::Gdi::*;

        unsafe {
            let mut count = 0usize;
            loop {
                let mut dd = DISPLAY_DEVICEW::default();
                dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                if !EnumDisplayDevicesW(None, count as u32, &mut dd, 0).as_bool() {
                    break;
                }
                count += 1;
            }
            count
        }
    }

    fn wait_for_display_devices(&mut self, desktop_state: &DesktopState) -> bool {
        let count = Self::display_device_count();
        if count > 0 {
            return true;
        }

        if self.last_display_discovery_log.elapsed() >= Duration::from_secs(2) {
            crate::service_win::svc_log(&format!(
                "display manager: waiting for Windows display stack; EnumDisplayDevicesW returned 0 devices on {:?}",
                desktop_state.phase()
            ));
            self.log_snapshot("display stack not ready", desktop_state);
            self.last_display_discovery_log = Instant::now();
        }
        false
    }

    fn rediscover_vdd_if_missing(&mut self, reason: &str) -> bool {
        if self.vdd_device.is_some() {
            return false;
        }
        if self.last_display_discovery_attempt.elapsed() < Duration::from_millis(750) {
            return false;
        }
        self.last_display_discovery_attempt = Instant::now();

        let _ = capture::gdi::switch_to_interactive_window_station();
        let _ = capture::gdi::switch_to_input_desktop();
        let Some(device) = find_vdd_device_name_quiet() else {
            if self.last_display_discovery_log.elapsed() >= Duration::from_secs(5) {
                crate::service_win::svc_log(&format!(
                    "display manager: VDD rediscovery still missing after {reason}; display_devices={}",
                    Self::display_device_count()
                ));
                self.last_display_discovery_log = Instant::now();
            }
            return false;
        };

        self.vdd_rect = display_ccd::active_source_rect(&device);
        self.vdd_device = Some(device.clone());
        self.vdd_primary_active = display_ccd::is_vdd_primary(&device);
        if self.capture_target_device.is_none() {
            self.capture_target_device = current_primary_display_device_name();
        }
        crate::service_win::svc_log(&format!(
            "display manager: rediscovered managed VDD {device} after {reason}; primary={} rect={:?}",
            self.vdd_primary_active, self.vdd_rect
        ));
        true
    }

    fn refresh_desktop(&mut self) -> DesktopState {
        let input_desktop = capture::gdi::current_input_desktop_name();
        let switch_ok = input_desktop.is_some() && capture::gdi::switch_to_input_desktop();
        let thread_desktop = capture::gdi::current_thread_desktop_name();
        let effective_desktop = input_desktop.clone().or_else(|| thread_desktop.clone());
        let changed_after_initial =
            self.desktop_sample_seen && effective_desktop != self.last_desktop_name;
        let transition_unavailable = effective_desktop.is_none()
            || (input_desktop.is_some() && !switch_ok && input_desktop != thread_desktop);
        if changed_after_initial
            || (transition_unavailable
                && self.last_desktop_transition_log.elapsed() >= Duration::from_secs(2))
        {
            crate::service_win::svc_log(&format!(
                "Input desktop state: input={:?} thread={:?} switch_ok={} previous={:?} - {}",
                input_desktop,
                thread_desktop,
                switch_ok,
                self.last_desktop_name,
                if transition_unavailable {
                    "holding capture during desktop transition"
                } else {
                    "resetting capture"
                }
            ));
            self.last_desktop_transition_log = Instant::now();
        }
        let on_default = effective_desktop
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("Default"));
        self.last_desktop_name = effective_desktop.clone();
        self.desktop_sample_seen = true;
        DesktopState {
            on_default,
            transition_unavailable,
            changed_after_initial,
            desktop_name: effective_desktop,
        }
    }

    fn decide_client_layout_request(
        &self,
        width: u32,
        height: u32,
        desktop_state: &DesktopState,
        capture_mode: &str,
    ) -> WindowsLayoutDecision {
        decide_layout_request(
            width,
            height,
            self.policy_for(desktop_state),
            self.topology_kind(),
            WindowsCapturePath::from_runtime_name(capture_mode),
            tier1_adaptive_enabled(),
        )
    }

    fn apply_managed_resolution(
        &mut self,
        new_w: u32,
        new_h: u32,
        width: &mut u32,
        height: &mut u32,
    ) -> bool {
        let Some(device) = self.vdd_device.clone() else {
            crate::service_win::svc_log(
                "display manager: refusing resolution change without managed VDD",
            );
            return false;
        };

        let changed = match display_ccd::set_source_resolution(&device, new_w, new_h) {
            Ok(topo) => {
                self.vdd_rect = display_ccd::source_rect_from_topology(&topo, &device);
                crate::service_win::svc_log(&format!(
                    "display manager: CCD source resolution applied to {device}: {new_w}x{new_h} rect={:?}",
                    self.vdd_rect
                ));
                true
            }
            Err(e) => {
                crate::service_win::svc_log(&format!(
                    "display manager: CCD source resolution failed for {device} {new_w}x{new_h}: {e:#}; trying legacy GDI mode switch"
                ));
                change_display_resolution_for_device(&device, new_w, new_h)
            }
        };

        if changed {
            *width = new_w;
            *height = new_h;
            self.repair_vdd_primary_after_resize();
            capture::gdi::nudge_desktop_repaint();
            true
        } else {
            false
        }
    }

    fn ensure_baseline_resolution(&mut self, width: &mut u32, height: &mut u32) {
        if tier1_adaptive_enabled() {
            return;
        }
        let Some(vdd_device) = self.managed_vdd_target() else {
            return;
        };
        let (target_w, target_h) = TIER1_BASELINE_RESOLUTION;
        if current_display_resolution(vdd_device) == Some((target_w, target_h)) {
            return;
        }
        crate::service_win::svc_log(&format!(
            "Tier 1 baseline: setting VDD to {}x{} before DXGI init",
            target_w, target_h
        ));
        if self.apply_managed_resolution(target_w, target_h, width, height) {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    fn repair_vdd_primary_after_resize(&mut self) {
        let Some(ref dev) = self.vdd_device else {
            return;
        };
        if !self.vdd_primary_active || display_ccd::is_vdd_primary(dev) {
            return;
        }
        match display_ccd::repair_sole_vdd_origin(dev) {
            Ok(topo) => {
                self.vdd_rect = display_ccd::source_rect_from_topology(&topo, dev);
                crate::service_win::svc_log("agent: re-applied CCD after resize (was broken)");
                capture::gdi::nudge_desktop_repaint();
            }
            Err(e) => crate::service_win::svc_log(&format!(
                "agent: re-apply CCD failed after resize: {e:#}"
            )),
        }
    }

    fn managed_capture_surface_mismatch(
        &self,
        actual_width: u32,
        actual_height: u32,
    ) -> Option<(String, u32, u32)> {
        let device = self.managed_vdd_target()?.to_string();
        let rect = self
            .vdd_rect
            .or_else(|| display_ccd::active_source_rect(&device));
        if capture_surface_matches_target(rect, actual_width, actual_height) {
            return None;
        }
        let (_, _, expected_width, expected_height) = rect?;
        Some((device, expected_width, expected_height))
    }

    fn reconcile_managed_capture_surface(
        &mut self,
        device: &str,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    ) {
        crate::service_win::svc_log(&format!(
            "display manager: DXGI surface {actual_width}x{actual_height} does not match managed CCD target {device} {expected_width}x{expected_height}; synchronizing legacy display mode before retry"
        ));
        let changed = change_display_resolution_for_device(device, expected_width, expected_height);
        self.refresh_capture_target();
        capture::gdi::nudge_desktop_repaint();
        crate::service_win::svc_log(if changed {
            "display manager: legacy display mode synchronized; rebuilding DXGI capture"
        } else {
            "display manager: legacy display mode is not synchronized yet; holding capture"
        });
    }
}

#[cfg(target_os = "windows")]
struct DefaultDesktopGate {
    candidate_since: Option<Instant>,
    access_denied_since: Option<Instant>,
    last_log: Instant,
}

#[cfg(target_os = "windows")]
impl DefaultDesktopGate {
    fn new() -> Self {
        Self {
            candidate_since: None,
            access_denied_since: None,
            last_log: instant_ago(Duration::from_secs(10)),
        }
    }

    fn reset(&mut self) {
        self.candidate_since = None;
        self.access_denied_since = None;
    }

    fn ready_to_capture(&mut self, desktop_state: &DesktopState) -> bool {
        if !desktop_state.on_default || desktop_state.transition_unavailable {
            self.reset();
            return false;
        }

        let since = *self.candidate_since.get_or_insert_with(Instant::now);
        if since.elapsed() < DEFAULT_DESKTOP_CAPTURE_STABLE_FOR {
            if self.last_log.elapsed() >= Duration::from_secs(2) {
                crate::service_win::svc_log(&format!(
                    "Default desktop candidate {:?}; waiting for capture readiness",
                    desktop_state.desktop_name
                ));
                self.last_log = Instant::now();
            }
            return false;
        }
        true
    }

    fn hold_after_access_denied(&mut self, reason: &str) -> bool {
        let since = *self.access_denied_since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_secs(6) {
            if self.last_log.elapsed() >= Duration::from_secs(1) {
                crate::service_win::svc_log(&format!(
                    "{reason}; treating as Default desktop not ready, retrying before fallback"
                ));
                self.last_log = Instant::now();
            }
            return true;
        }
        crate::service_win::svc_log(&format!(
            "{reason}; Default readiness retry window expired, allowing same-target fallback"
        ));
        self.access_denied_since = None;
        false
    }

    fn mark_capture_ready(&mut self) {
        self.access_denied_since = None;
    }
}

#[cfg(target_os = "windows")]
fn is_windows_access_denied(error: &anyhow::Error) -> bool {
    let msg = format!("{error:#}");
    let lower = msg.to_ascii_lowercase();
    lower.contains("0x80070005") || lower.contains("access is denied")
}

#[cfg(target_os = "windows")]
const DEFAULT_DESKTOP_CAPTURE_STABLE_FOR: Duration = Duration::from_millis(500);

#[cfg(target_os = "windows")]
fn ensure_windows_tier1_baseline_resolution(
    display_state: &mut WindowsDisplayManager,
    width: &mut u32,
    height: &mut u32,
) {
    display_state.ensure_baseline_resolution(width, height);
}

#[cfg(target_os = "windows")]
fn run_agent_loop(
    ipc: &ipc_pipe::IpcClient,
    injector: &mut Option<input_injector::InputInjector>,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    assigned_desktop: Option<WindowsAgentDesktop>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use phantom_core::capture::FrameCapture;
    use phantom_core::encode::FrameEncoder;
    use phantom_core::input::InputEvent;
    use phantom_core::protocol::{CursorShape, CursorState};

    let frame_interval = Duration::from_secs_f64(1.0 / 30.0);
    let idle_pipeline_ttl = Duration::from_secs(60);

    // Do not initialize arboard/clipboard in the agent capture thread. On
    // Windows, clipboard/user32 helpers may create thread-owned windows, after
    // which SetThreadDesktop can no longer follow Winlogon <-> Default.
    let mut arboard: Option<arboard::Clipboard> = None;
    let mut last_clipboard = String::new();
    let mut clipboard_poll = Instant::now();

    let mut display_state = WindowsDisplayManager::new();

    // Capture tiers (best to worst), always bound to this agent's immutable
    // active target:
    // 1. DXGI -> NVENC zero-copy (GPU capture + GPU encode, ~4ms)
    // 2. DXGI -> CPU encode
    // 3. GDI -> CPU encode (secure-desktop or same-target fallback)
    let mut gpu_pipeline: Option<phantom_gpu::dxgi_nvenc::DxgiNvencPipeline> = None;
    let mut pending_gpu_pipeline: Option<phantom_gpu::dxgi_nvenc::DxgiNvencPipeline> = None;
    let mut scrap_capture: Option<capture::scrap::ScrapCapture> = None;
    let mut gdi_capture: Option<capture::gdi::GdiCapture> = None;
    let mut cpu_encoder: Option<Box<dyn FrameEncoder>> = None;

    let mut frame_count = 0u64;
    let mut last_keyframe = Instant::now();
    let mut last_init_attempt = instant_ago(Duration::from_secs(10));
    let mut width = 1920u32;
    let mut height = 1080u32;
    let mut capture_mode = "none";
    let mut pending_resolution_request: Option<(u32, u32)> = None;
    let mut last_deferred_resolution_log = instant_ago(Duration::from_secs(10));
    let mut last_deferred_resolution_request: Option<(u32, u32)> = None;
    let mut scrap_waiting_for_frame_since: Option<Instant> = None;
    let mut gpu_recovery = Tier1StartupRecovery::default();
    // Display offset on virtual desktop — needed to map mouse coordinates
    // when capturing from a secondary display (e.g. VDD).
    let mut display_x: i32 = 0;
    let mut display_y: i32 = 0;
    let mut last_input_summary_log = instant_ago(Duration::from_secs(10));
    let mut input_events_since_log = 0usize;
    let mut last_input_detail_log = instant_ago(Duration::from_secs(10));
    let mut last_mouse_move_log = instant_ago(Duration::from_secs(10));
    let mut last_mouse_pos: Option<(i32, i32)> = None;
    let mut last_cursor_state: Option<CursorState> = None;
    let mut last_cursor_state_sent = instant_ago(Duration::from_secs(1));
    let mut last_cursor_handle: usize = 0;
    let mut last_cursor_shape: Option<CursorShape> = None;
    let mut last_cursor_shape_failed_handle: usize = 0;
    let mut last_viewer_active = false;
    let mut viewer_idle_since: Option<Instant> = None;
    let mut gpu_prewarm_ready = false;
    let mut gpu_prewarm_started_at: Option<Instant> = None;
    let mut idle_gpu_prewarm_suspended = false;
    let mut last_gpu_prewarm_log = instant_ago(Duration::from_secs(10));
    let mut last_pending_gpu_probe = instant_ago(Duration::from_secs(10));
    let mut last_assignment_hold_log = instant_ago(Duration::from_secs(10));
    let mut gdi_probe_count = 0u64;
    let mut default_desktop_gate = DefaultDesktopGate::new();
    tracing::info!("Starting agent loop");

    while !shutdown.load(Ordering::Relaxed) && !ipc.should_shutdown() {
        let loop_start = Instant::now();
        let viewer_active = ipc.viewer_active();
        if viewer_active != last_viewer_active {
            crate::service_win::svc_log(if viewer_active {
                "viewer active: resuming capture"
            } else {
                "viewer idle: pausing capture"
            });
            last_viewer_active = viewer_active;
            viewer_idle_since = if viewer_active {
                idle_gpu_prewarm_suspended = false;
                // Cursor shape is session state for the viewer, not just an OS
                // handle-change event. Force a resend for each newly attached
                // viewer so clients do not depend on stale IPC timing.
                last_cursor_handle = 0;
                last_cursor_shape = None;
                last_cursor_shape_failed_handle = 0;
                None
            } else {
                Some(Instant::now())
            };
        }
        if let Some(assigned) = assigned_desktop {
            let input_desktop = capture::gdi::current_input_desktop_name();
            if !assigned.matches_input_desktop(input_desktop.as_deref()) {
                // The service deliberately keeps the previous generation alive
                // until its replacement has produced a valid keyframe. Freeze
                // that old generation instead of letting it capture the new
                // desktop while its display topology is still being prepared.
                if last_assignment_hold_log.elapsed() >= Duration::from_secs(1) {
                    crate::service_win::svc_log(&format!(
                        "agent generation desktop hold: assigned={assigned:?} input={input_desktop:?}; preserving last frame for handoff"
                    ));
                    last_assignment_hold_log = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        if !viewer_active {
            let should_prewarm_gpu = (!gpu_prewarm_ready && gpu_pipeline.is_some())
                || (!idle_gpu_prewarm_suspended
                    && !gpu_prewarm_ready
                    && gpu_pipeline.is_none()
                    && scrap_capture.is_none()
                    && gdi_capture.is_none()
                    && cpu_encoder.is_none()
                    && gpu_recovery.can_try_tier1());

            if should_prewarm_gpu {
                let idle_desktop = capture::gdi::current_input_desktop_name()
                    .or_else(capture::gdi::current_thread_desktop_name);
                if idle_desktop
                    .as_deref()
                    .is_none_or(|name| !name.eq_ignore_ascii_case("Default"))
                {
                    idle_gpu_prewarm_suspended = true;
                    gpu_prewarm_started_at = None;
                    if last_gpu_prewarm_log.elapsed() >= Duration::from_secs(5) {
                        crate::service_win::svc_log(&format!(
                            "viewer idle: skipping Tier 1 prewarm on desktop {:?}",
                            idle_desktop
                        ));
                        last_gpu_prewarm_log = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let prewarm_started = gpu_prewarm_started_at.get_or_insert_with(Instant::now);
                if last_gpu_prewarm_log.elapsed() >= Duration::from_secs(5) {
                    crate::service_win::svc_log(
                        "viewer idle: prewarming Tier 1 capture until first GPU frame",
                    );
                    last_gpu_prewarm_log = Instant::now();
                }
                if gpu_pipeline.is_some() && prewarm_started.elapsed() > Duration::from_secs(5) {
                    gpu_pipeline = None;
                    gpu_prewarm_ready = false;
                    gpu_prewarm_started_at = None;
                    idle_gpu_prewarm_suspended = true;
                    gpu_recovery.clear_startup_wait();
                    last_init_attempt = Instant::now();
                    crate::service_win::svc_log(
                        "viewer idle: Tier 1 prewarm saw no desktop update; parking Tier 1 until viewer/desktop activity",
                    );
                    continue;
                }
            } else if gpu_pipeline.is_some() && gpu_prewarm_ready {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            } else if gpu_pipeline.is_some()
                || scrap_capture.is_some()
                || gdi_capture.is_some()
                || cpu_encoder.is_some()
            {
                let idle_since = viewer_idle_since.get_or_insert_with(Instant::now);
                if idle_since.elapsed() >= idle_pipeline_ttl {
                    gpu_pipeline = None;
                    pending_gpu_pipeline = None;
                    scrap_capture = None;
                    gdi_capture = None;
                    cpu_encoder = None;
                    gpu_prewarm_ready = false;
                    gpu_prewarm_started_at = None;
                    idle_gpu_prewarm_suspended = true;
                    scrap_waiting_for_frame_since = None;
                    // Preserve tier1_disabled across idle. If this VM already
                    // proved DXGI/VDD no-frame, the next viewer should not wait
                    // another 10+ seconds before reaching GDI fallback.
                    gpu_recovery.clear_startup_wait();
                    capture_mode = "none";
                    crate::service_win::svc_log(
                        "viewer idle TTL: released capture pipeline; keeping display topology",
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            } else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }

        // Detect desktop switch (Winlogon ↔ Default): if changed, force reset
        // of all capture pipelines so the new duplication targets the right
        // desktop. Without this, after lock→unlock the client keeps seeing
        // the login screen until the user manually reconnects.
        let desktop_state = display_state.refresh_desktop();
        let mut display_policy = display_state.policy_for(&desktop_state);
        if desktop_state.changed_after_initial {
            display_state.log_snapshot("desktop transition observed", &desktop_state);
            gpu_pipeline = None;
            pending_gpu_pipeline = None;
            scrap_capture = None;
            gdi_capture = None;
            cpu_encoder = None;
            gpu_prewarm_ready = false;
            gpu_prewarm_started_at = None;
            idle_gpu_prewarm_suspended = false;
            gpu_recovery.reset();
            default_desktop_gate.reset();
            last_init_attempt = instant_ago(Duration::from_secs(10));
        }

        if display_policy.holds_capture() {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if !display_policy.uses_console_capture()
            && !default_desktop_gate.ready_to_capture(&desktop_state)
        {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if !display_state.wait_for_display_devices(&desktop_state) {
            gpu_pipeline = None;
            pending_gpu_pipeline = None;
            scrap_capture = None;
            gdi_capture = None;
            cpu_encoder = None;
            gpu_prewarm_ready = false;
            gpu_prewarm_started_at = None;
            idle_gpu_prewarm_suspended = false;
            scrap_waiting_for_frame_since = None;
            last_init_attempt = instant_ago(Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if display_state.rediscover_vdd_if_missing("display stack became ready") {
            gpu_pipeline = None;
            pending_gpu_pipeline = None;
            scrap_capture = None;
            gdi_capture = None;
            cpu_encoder = None;
            gpu_prewarm_ready = false;
            gpu_prewarm_started_at = None;
            idle_gpu_prewarm_suspended = false;
            scrap_waiting_for_frame_since = None;
            gpu_recovery.reset();
            last_init_attempt = instant_ago(Duration::from_secs(10));
        }
        if !display_state.ensure_provisioning_ready(&desktop_state) {
            gpu_pipeline = None;
            pending_gpu_pipeline = None;
            scrap_capture = None;
            gdi_capture = None;
            cpu_encoder = None;
            capture_mode = "none";
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        display_policy = display_state.policy_for(&desktop_state);

        // Handle resolution change requests before capture init. A new viewer
        // sends its viewport hint before requesting the first keyframe; if the
        // agent initializes DXGI first and only then applies the hint, it
        // creates a stale old-mode pipeline and immediately tears it down.
        if let Some(request) = ipc.take_resolution_request() {
            pending_resolution_request = Some(request);
        }
        if let Some((requested_w, requested_h)) = pending_resolution_request {
            match display_state.decide_client_layout_request(
                requested_w,
                requested_h,
                &desktop_state,
                capture_mode,
            ) {
                WindowsLayoutDecision::Deferred { reason, retry } => {
                    if last_deferred_resolution_request != Some((requested_w, requested_h))
                        || last_deferred_resolution_log.elapsed() >= Duration::from_secs(1)
                    {
                        crate::service_win::svc_log(&format!(
                            "display manager: deferring resolution request {requested_w}x{requested_h}: {reason}"
                        ));
                        display_state.log_snapshot("resolution deferred", &desktop_state);
                        last_deferred_resolution_request = Some((requested_w, requested_h));
                        last_deferred_resolution_log = Instant::now();
                    }
                    if !retry {
                        pending_resolution_request = None;
                    }
                }
                WindowsLayoutDecision::Denied { reason } => {
                    crate::service_win::svc_log(&format!(
                        "display manager: denying resolution request {requested_w}x{requested_h}: {reason}"
                    ));
                    display_state.log_snapshot("resolution denied", &desktop_state);
                    pending_resolution_request = None;
                }
                WindowsLayoutDecision::Apply {
                    width: new_w,
                    height: new_h,
                } => {
                    pending_resolution_request = None;
                    last_deferred_resolution_request = None;
                    if new_w != width || new_h != height {
                        display_state.log_snapshot("resolution apply before", &desktop_state);
                        tracing::info!(
                            old_w = width,
                            old_h = height,
                            new_w,
                            new_h,
                            "Resolution change requested"
                        );
                        // CRITICAL ORDER: drop capture pipelines BEFORE changing display
                        // mode. Otherwise the old pipeline captures the brief black
                        // transition the desktop goes through during mode switch and
                        // sends those black frames to the client — that's the "black
                        // flash" users see. With pipeline dropped first, no frames are
                        // sent during the transition and client keeps the last frame.
                        gpu_pipeline = None;
                        pending_gpu_pipeline = None;
                        scrap_capture = None;
                        gdi_capture = None;
                        cpu_encoder = None;
                        gpu_prewarm_ready = false;
                        gpu_prewarm_started_at = None;
                        idle_gpu_prewarm_suspended = false;
                        scrap_waiting_for_frame_since = None;
                        gpu_recovery.clear_startup_wait();

                        if display_state.apply_managed_resolution(
                            new_w,
                            new_h,
                            &mut width,
                            &mut height,
                        ) {
                            display_state.log_snapshot("resolution apply after", &desktop_state);
                            last_init_attempt = instant_ago(Duration::from_secs(10));
                            // Brief pause for Windows to settle. 200ms is enough for
                            // DXGI/GDI to reflect the new mode; 500ms was overly safe.
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            }
        }

        // Try to init/reinit capture pipeline (best available)
        let needs_capture_init = gpu_pipeline.is_none()
            && scrap_capture.is_none()
            && gdi_capture.is_none()
            && last_init_attempt.elapsed() > Duration::from_secs(1);
        if needs_capture_init && display_policy.uses_console_capture() {
            last_init_attempt = Instant::now();
            crate::service_win::svc_log(
                "Winlogon desktop: using stable-target GDI+OpenH264 capture",
            );
            // Capture the secure desktop using its own primary-screen metrics.
            // After sign-out, CCD can still advertise the previous 1920x1080
            // VDD while LogonUI's actual surface is 800x600. Creating a DC for
            // that stale device rect pads the login screen with black pixels.
            // GetSystemMetrics on the Winlogon desktop reports the drawable
            // surface and still resolves to the VDD size for an ordinary lock.
            scrap_capture = None;
            scrap_waiting_for_frame_since = None;
            gpu_prewarm_ready = false;
            gpu_prewarm_started_at = None;
            activate_gdi_fallback_region(
                &mut cpu_encoder,
                &mut gdi_capture,
                &mut width,
                &mut height,
                &mut display_x,
                &mut display_y,
                &mut capture_mode,
                false,
                capture::gdi::GdiCaptureRegion::PrimaryMonitor,
            );
        } else if needs_capture_init {
            last_init_attempt = Instant::now();

            if !gpu_recovery.can_try_tier1() {
                crate::service_win::svc_log(
                    "Tier 1 disabled after startup failure; trying same-target CPU fallback",
                );
                scrap_capture = None;
                scrap_waiting_for_frame_since = None;
                gpu_prewarm_ready = false;
                gpu_prewarm_started_at = None;
                activate_cpu_fallback(
                    "Tier 1 disabled; same-target CPU fallback",
                    &mut cpu_encoder,
                    &mut scrap_capture,
                    &mut gdi_capture,
                    &mut width,
                    &mut height,
                    &mut display_x,
                    &mut display_y,
                    &mut capture_mode,
                    &mut scrap_waiting_for_frame_since,
                    display_state.capture_target_device(),
                    display_state.capture_target_rect(),
                    true,
                );
                continue;
            }

            ensure_windows_tier1_baseline_resolution(&mut display_state, &mut width, &mut height);

            // Tier 1: capture the immutable target selected when this agent
            // started. An explicit target is strict: DXGI must never substitute
            // a different NVIDIA output and silently stream another desktop.
            let tier1_init = (|| -> anyhow::Result<phantom_gpu::dxgi_nvenc::DxgiNvencPipeline> {
                let gpu = phantom_gpu::dxgi_nvenc::DxgiNvencPipeline::with_target_device(
                    30,
                    5000,
                    display_state.capture_target_device(),
                )?;
                crate::service_win::svc_log(&format!(
                    "Tier 1 DXGI stable target: {}",
                    gpu.capture.target_summary()
                ));
                Ok(gpu)
            })();

            match tier1_init {
                Ok(mut gpu) => {
                    if display_state.invalidate_if_ready_topology_drifted(&desktop_state) {
                        drop(gpu);
                        gpu_recovery.clear_startup_wait();
                        last_init_attempt = instant_ago(Duration::from_secs(10));
                        continue;
                    }
                    if let Some((device, expected_width, expected_height)) =
                        display_state.managed_capture_surface_mismatch(gpu.width, gpu.height)
                    {
                        let actual_width = gpu.width;
                        let actual_height = gpu.height;
                        drop(gpu);
                        display_state.reconcile_managed_capture_surface(
                            &device,
                            expected_width,
                            expected_height,
                            actual_width,
                            actual_height,
                        );
                        last_init_attempt = instant_ago(Duration::from_secs(10));
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    width = gpu.width;
                    height = gpu.height;
                    let need_startup_frame = frame_count == 0 && !gpu_prewarm_ready;
                    if need_startup_frame {
                        // The duplication object was just created. Recreating it
                        // for every startup retry can keep a login-transition
                        // frame alive forever, so request an IDR and generate a
                        // throttled desktop update instead.
                        let now = Instant::now();
                        gpu.force_keyframe();
                        gpu_recovery.wait_for_frame(now);
                        if gpu_recovery.should_nudge_startup(now) {
                            nudge_windows_capture_target(
                                display_state.capture_target_device(),
                                display_state.capture_target_rect(),
                            );
                        }
                    } else {
                        // We already delivered a valid startup frame. A later
                        // DXGI reinit can legitimately see no desktop changes;
                        // keep the last frame instead of treating static
                        // desktop as a failure and falling into unsafe GDI.
                        gpu.force_keyframe();
                    }
                    crate::service_win::svc_log(&format!(
                        "Tier 1: DXGI→NVENC ready {}x{}",
                        width, height
                    ));
                    gpu_pipeline = Some(gpu);
                    pending_gpu_pipeline = None;
                    scrap_capture = None;
                    gdi_capture = None;
                    cpu_encoder = None;
                    gpu_prewarm_ready = false;
                    gpu_prewarm_started_at = None;
                    scrap_waiting_for_frame_since = None;
                    if need_startup_frame {
                        // Validate startup frames even during idle prewarm.
                        // Without this, a Winlogon -> Default transition can
                        // enqueue a black prewarmed keyframe that the next
                        // viewer accepts as live.
                    } else {
                        gpu_recovery.clear_startup_wait();
                    }
                    capture_mode = "dxgi_nvenc";
                }
                Err(e) => {
                    gpu_prewarm_ready = false;
                    gpu_prewarm_started_at = None;
                    if !viewer_active {
                        idle_gpu_prewarm_suspended = true;
                        crate::service_win::svc_log(&format!(
                            "viewer idle: Tier 1 prewarm unavailable on desktop {:?}: {e:#}",
                            desktop_state.desktop_name
                        ));
                        continue;
                    }
                    let reason = format!("Tier 1 DXGI/NVENC unavailable: {e:#}");
                    if is_windows_access_denied(&e)
                        && default_desktop_gate.hold_after_access_denied(&reason)
                    {
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    if gpu_recovery.consume_startup_retry() {
                        crate::service_win::svc_log(&format!(
                            "{reason}; retrying Tier 1 once before CPU fallback"
                        ));
                        last_init_attempt = Instant::now();
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    gpu_recovery.disable_tier1();
                    activate_cpu_fallback(
                        &reason,
                        &mut cpu_encoder,
                        &mut scrap_capture,
                        &mut gdi_capture,
                        &mut width,
                        &mut height,
                        &mut display_x,
                        &mut display_y,
                        &mut capture_mode,
                        &mut scrap_waiting_for_frame_since,
                        display_state.capture_target_device(),
                        display_state.capture_target_rect(),
                        true,
                    );
                    if scrap_capture.is_none() && gdi_capture.is_none() && frame_count == 0 {
                        tracing::warn!("All capture methods failed");
                    }
                }
            }
        }

        // Handle keyframe requests from service (new session).
        if ipc.take_keyframe_request() {
            tracing::debug!("Agent: received keyframe request from service");
            // Keep keyframe requests side-effect-free with respect to display
            // topology. Service/viewer retries can arrive while Tier 1 is
            // still initializing; reapplying CCD here invalidates Desktop
            // Duplication/Scrap and causes black startup loops. Display state
            // repair belongs to desktop/resize/init transitions above.
            if let Some(ref mut gpu) = gpu_pipeline {
                if gpu_prewarm_ready {
                    // DXGI only returns frames when the desktop/cursor changes.
                    // Preserve the live duplication object and ask DWM for a
                    // repaint so the requested IDR is emitted on a static
                    // desktop without moving the user's cursor.
                    gpu.force_keyframe();
                    capture::gdi::nudge_desktop_repaint();
                    gpu_recovery.clear_startup_wait();
                } else {
                    let now = Instant::now();
                    gpu.force_keyframe();
                    gpu_recovery.wait_for_frame(now);
                    if gpu_recovery.should_nudge_startup(now) {
                        nudge_windows_capture_target(
                            display_state.capture_target_device(),
                            display_state.capture_target_rect(),
                        );
                    }
                }
            }
            if let Some(ref mut gpu) = pending_gpu_pipeline {
                gpu.force_keyframe();
            }
            if scrap_capture.is_some() {
                // Do not reset Scrap/DXGI on every viewer keyframe nudge.
                // Service retries every 500ms during startup; repeated reset
                // invalidates the duplication object before it can emit the
                // first frame. Nudge the desktop instead and let the encoder
                // keyframe flag below apply to the next captured frame.
                nudge_windows_capture_target(
                    display_state.capture_target_device(),
                    display_state.capture_target_rect(),
                );
            }
            if let Some(ref mut enc) = cpu_encoder {
                enc.force_keyframe();
            }
            last_keyframe = Instant::now();
        }
        // Periodic keyframe (2s) — only marks encoder, does NOT reset capture.
        // On static desktop, this is a no-op (no frame to encode). That's fine
        // because the client already has the last keyframe.
        if last_keyframe.elapsed() > Duration::from_secs(2) {
            if let Some(ref mut gpu) = gpu_pipeline {
                gpu.force_keyframe();
            }
            if let Some(ref mut enc) = cpu_encoder {
                enc.force_keyframe();
            }
            last_keyframe = Instant::now();
        }

        // If Tier 1 could not produce the initial static-desktop frame, GDI is
        // used as a bootstrap path while DXGI stays alive here. Once user
        // activity makes Desktop Duplication emit a real frame, promote back to
        // zero-copy without forcing another reconnect.
        if gpu_pipeline.is_none()
            && pending_gpu_pipeline.is_some()
            && last_pending_gpu_probe.elapsed() >= Duration::from_millis(100)
        {
            last_pending_gpu_probe = Instant::now();
            let mut recovered_gpu_frame = None;
            let mut drop_pending_gpu = false;
            if let Some(ref mut pending_gpu) = pending_gpu_pipeline {
                match pending_gpu.capture_and_encode() {
                    Ok(Some(encoded)) => {
                        let mostly_black = pending_gpu
                            .capture
                            .sample_bgra_stats(2048)
                            .map(|stats| stats.is_mostly_black())
                            .unwrap_or(false);
                        if mostly_black {
                            pending_gpu.force_keyframe();
                        } else {
                            recovered_gpu_frame =
                                Some((encoded, pending_gpu.width, pending_gpu.height));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::service_win::svc_log(&format!(
                            "Pending Tier 1 DXGI/NVENC failed while GDI bootstrap active: {e:#}"
                        ));
                        drop_pending_gpu = true;
                    }
                }
            }
            if drop_pending_gpu {
                pending_gpu_pipeline = None;
            }
            if let Some((encoded, gpu_w, gpu_h)) = recovered_gpu_frame {
                width = gpu_w;
                height = gpu_h;
                display_x = 0;
                display_y = 0;
                capture_mode = "dxgi_nvenc";
                frame_count += 1;
                if encoded.is_keyframe {
                    last_keyframe = Instant::now();
                }
                crate::service_win::svc_log(&format!(
                    "Tier 1 DXGI/NVENC recovered after GDI bootstrap {}x{} keyframe={} bytes={}",
                    width,
                    height,
                    encoded.is_keyframe,
                    encoded.data.len()
                ));
                if let Err(e) = ipc.send_encoded_frame(&encoded, width, height) {
                    tracing::error!("IPC send failed: {e}");
                    break;
                }
                gpu_pipeline = pending_gpu_pipeline.take();
                scrap_capture = None;
                gdi_capture = None;
                cpu_encoder = None;
                scrap_waiting_for_frame_since = None;
                gpu_prewarm_ready = true;
                gpu_recovery.mark_frame_ready();
                continue;
            }
        }

        // Capture + encode: Tier 1 — GPU path (DXGI→NVENC zero-copy)
        if let Some(ref mut gpu) = gpu_pipeline {
            match gpu.capture_and_encode() {
                Ok(Some(encoded)) => {
                    if let Some(since) = gpu_recovery.waiting_for_frame_since() {
                        let now = Instant::now();
                        match gpu.capture.sample_bgra_stats(2048) {
                            Ok(stats) if stats.is_mostly_black() => {
                                if gpu_recovery.should_log_black_startup() {
                                    crate::service_win::svc_log(&format!(
                                        "Tier 1 DXGI/NVENC produced black startup frame {}x{} black_pct={} mean_rgb={},{},{}",
                                        width,
                                        height,
                                        stats.black_pct,
                                        stats.mean_r,
                                        stats.mean_g,
                                        stats.mean_b
                                    ));
                                }
                                if since.elapsed() > Duration::from_millis(2500) {
                                    gpu_pipeline = None;
                                    pending_gpu_pipeline = None;
                                    gpu_prewarm_ready = false;
                                    gpu_prewarm_started_at = None;
                                    if gpu_recovery.consume_startup_retry() {
                                        crate::service_win::svc_log(
                                            "Tier 1 DXGI/NVENC stayed black after startup; reinitializing Tier 1 before CPU fallback",
                                        );
                                        last_init_attempt = instant_ago(Duration::from_secs(10));
                                    } else {
                                        gpu_recovery.disable_tier1();
                                        crate::service_win::svc_log(
                                            "Tier 1 DXGI/NVENC stayed black after startup; trying same-target CPU/GDI fallback",
                                        );
                                        scrap_capture = None;
                                        scrap_waiting_for_frame_since = None;
                                        activate_cpu_fallback(
                                            "Tier 1 DXGI/NVENC black startup; same-target CPU fallback",
                                            &mut cpu_encoder,
                                            &mut scrap_capture,
                                            &mut gdi_capture,
                                            &mut width,
                                            &mut height,
                                            &mut display_x,
                                            &mut display_y,
                                            &mut capture_mode,
                                            &mut scrap_waiting_for_frame_since,
                                            display_state.capture_target_device(),
                                            display_state.capture_target_rect(),
                                            true,
                                        );
                                    }
                                } else {
                                    gpu.force_keyframe();
                                    if gpu_recovery.should_nudge_startup(now) {
                                        nudge_windows_capture_target(
                                            display_state.capture_target_device(),
                                            display_state.capture_target_rect(),
                                        );
                                    }
                                }
                                continue;
                            }
                            Ok(_) => {
                                gpu_recovery.mark_frame_ready();
                                default_desktop_gate.mark_capture_ready();
                            }
                            Err(e) => {
                                crate::service_win::svc_log(&format!(
                                    "Tier 1 DXGI/NVENC startup frame sample failed: {e:#}"
                                ));
                                gpu_recovery.mark_frame_ready();
                                default_desktop_gate.mark_capture_ready();
                            }
                        }
                    }
                    frame_count += 1;
                    if frame_count <= 3 || frame_count.is_multiple_of(300) {
                        tracing::info!(
                            frame = frame_count,
                            width,
                            height,
                            bytes = encoded.data.len(),
                            keyframe = encoded.is_keyframe,
                            "GPU frame"
                        );
                    }
                    if encoded.is_keyframe {
                        last_keyframe = Instant::now();
                    }
                    if let Err(e) = ipc.send_encoded_frame(&encoded, width, height) {
                        tracing::error!("IPC send failed: {e}");
                        break;
                    }
                    if !gpu_prewarm_ready {
                        gpu_prewarm_ready = true;
                        gpu_prewarm_started_at = None;
                        if !viewer_active {
                            gpu_recovery.clear_startup_wait();
                            crate::service_win::svc_log(&format!(
                                "viewer idle: Tier 1 prewarm produced first GPU frame {}x{} keyframe={} bytes={}",
                                width,
                                height,
                                encoded.is_keyframe,
                                encoded.data.len()
                            ));
                        }
                    }
                }
                Ok(None) => {
                    if gpu_recovery.startup_wait_timed_out(Instant::now()) {
                        gpu_pipeline = None;
                        pending_gpu_pipeline = None;
                        gpu_prewarm_ready = false;
                        scrap_capture = None;
                        scrap_waiting_for_frame_since = None;
                        gpu_prewarm_started_at = None;
                        nudge_windows_capture_target(
                            display_state.capture_target_device(),
                            display_state.capture_target_rect(),
                        );
                        if gpu_recovery.consume_startup_retry() {
                            crate::service_win::svc_log(
                                "Tier 1 DXGI/NVENC produced no frame during startup; reinitializing DXGI on settled Default desktop",
                            );
                            last_init_attempt = instant_ago(Duration::from_secs(10));
                        } else {
                            gpu_recovery.disable_tier1();
                            crate::service_win::svc_log(
                                "Tier 1 DXGI/NVENC produced no frame during startup; trying same-target CPU/GDI fallback",
                            );
                            activate_cpu_fallback(
                                "Tier 1 DXGI/NVENC no startup frame; same-target CPU fallback",
                                &mut cpu_encoder,
                                &mut scrap_capture,
                                &mut gdi_capture,
                                &mut width,
                                &mut height,
                                &mut display_x,
                                &mut display_y,
                                &mut capture_mode,
                                &mut scrap_waiting_for_frame_since,
                                display_state.capture_target_device(),
                                display_state.capture_target_rect(),
                                true,
                            );
                        }
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    tracing::warn!("DXGI→NVENC error: {e:#}, resetting pipeline");
                    gpu_pipeline = None;
                    pending_gpu_pipeline = None;
                    gpu_prewarm_ready = false;
                    gpu_prewarm_started_at = None;
                    gpu_recovery.reset();
                    // Cooldown before retrying Tier 1 — let Tier 2/3 take over.
                    // Without this, ACCESS_LOST on lock screen causes infinite
                    // Tier 1 init→fail loop that never reaches Tier 2/3.
                    last_init_attempt = Instant::now();
                    if is_windows_access_denied(&e) {
                        let reason = format!("Tier 1 DXGI/NVENC runtime access denied: {e:#}");
                        let _ = default_desktop_gate.hold_after_access_denied(&reason);
                    }
                }
            }
        }
        // Capture + encode: Tier 2 — ScrapCapture (DXGI CPU) + OpenH264
        else if scrap_capture.is_some() && cpu_encoder.is_some() {
            match scrap_capture
                .as_mut()
                .expect("scrap_capture checked")
                .capture()
            {
                Ok(Some(frame)) => match cpu_encoder
                    .as_mut()
                    .expect("cpu_encoder checked")
                    .encode_frame(&frame)
                {
                    Ok(encoded) => {
                        frame_count += 1;
                        if frame_count <= 3 || frame_count.is_multiple_of(300) {
                            tracing::info!(
                                frame = frame_count,
                                width,
                                height,
                                bytes = encoded.data.len(),
                                keyframe = encoded.is_keyframe,
                                mode = capture_mode,
                                "DXGI CPU frame"
                            );
                            crate::service_win::svc_log(&format!(
                                "{} frame {}: {}x{} keyframe={} bytes={}",
                                capture_mode,
                                frame_count,
                                width,
                                height,
                                encoded.is_keyframe,
                                encoded.data.len()
                            ));
                        }
                        if encoded.is_keyframe {
                            last_keyframe = Instant::now();
                        }
                        if let Err(e) = ipc.send_encoded_frame(&encoded, width, height) {
                            tracing::error!("IPC send failed: {e}");
                            break;
                        }
                        scrap_waiting_for_frame_since = None;
                    }
                    Err(e) => {
                        crate::service_win::svc_log(&format!("ScrapCapture encode error: {e:#}"));
                        tracing::warn!("ScrapCapture encode error: {e}");
                    }
                },
                Ok(None) => {
                    if scrap_waiting_for_frame_since
                        .is_some_and(|since| since.elapsed() > Duration::from_millis(750))
                    {
                        scrap_capture = None;
                        cpu_encoder = None;
                        scrap_waiting_for_frame_since = None;
                        if display_policy.allows_managed_tier1_retry()
                            && gpu_recovery.can_try_tier1()
                        {
                            crate::service_win::svc_log(
                                "ScrapCapture stalled on managed Default VDD; parking CPU fallback and retrying Tier 1 (GDI disabled)",
                            );
                            gpu_recovery.reset();
                            last_init_attempt = instant_ago(Duration::from_secs(10));
                            capture_mode = "none";
                            continue;
                        }
                        tracing::warn!(
                            width,
                            height,
                            "ScrapCapture produced no frame after reset/init, switching to same-target GDI"
                        );
                        crate::service_win::svc_log(
                            "ScrapCapture stalled after reset/init; switching to same-target GDI",
                        );
                        activate_gdi_fallback(
                            &mut cpu_encoder,
                            &mut gdi_capture,
                            &mut width,
                            &mut height,
                            &mut display_x,
                            &mut display_y,
                            &mut capture_mode,
                            display_state.capture_target_device(),
                            true,
                        );
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    scrap_capture = None;
                    cpu_encoder = None;
                    scrap_waiting_for_frame_since = None;
                    if display_policy.allows_managed_tier1_retry() && gpu_recovery.can_try_tier1() {
                        crate::service_win::svc_log(&format!(
                                "ScrapCapture error: {e:#}; parking CPU fallback and retrying Tier 1 (Default VDD GDI disabled)"
                            ));
                        gpu_recovery.reset();
                        last_init_attempt = instant_ago(Duration::from_secs(10));
                        capture_mode = "none";
                        continue;
                    }
                    crate::service_win::svc_log(&format!(
                        "ScrapCapture error: {e:#}; switching to same-target GDI"
                    ));
                    tracing::warn!("ScrapCapture error: {e}, switching to same-target GDI");
                    activate_gdi_fallback(
                        &mut cpu_encoder,
                        &mut gdi_capture,
                        &mut width,
                        &mut height,
                        &mut display_x,
                        &mut display_y,
                        &mut capture_mode,
                        display_state.capture_target_device(),
                        true,
                    );
                }
            }
        }
        // Capture + encode: Tier 3 — GDI fallback (lock screen, no display)
        else if let (Some(ref mut gdi), Some(ref mut enc)) = (&mut gdi_capture, &mut cpu_encoder)
        {
            gdi_probe_count += 1;
            if gdi_probe_count <= 3 {
                crate::service_win::svc_log(&format!(
                    "GDI capture attempt {}: {}x{} mode={}",
                    gdi_probe_count, width, height, capture_mode
                ));
            }
            let capture_started = Instant::now();
            match gdi.capture() {
                Ok(Some(frame)) => {
                    let capture_elapsed = capture_started.elapsed();
                    if gdi_probe_count <= 3 || capture_elapsed > Duration::from_millis(250) {
                        crate::service_win::svc_log(&format!(
                            "GDI capture produced frame: attempt={} capture_ms={}",
                            gdi_probe_count,
                            capture_elapsed.as_millis()
                        ));
                    }
                    let encode_started = Instant::now();
                    match enc.encode_frame(&frame) {
                        Ok(encoded) => {
                            let encode_elapsed = encode_started.elapsed();
                            frame_count += 1;
                            if frame_count <= 3 || frame_count.is_multiple_of(300) {
                                tracing::info!(
                                    frame = frame_count,
                                    width,
                                    height,
                                    bytes = encoded.data.len(),
                                    keyframe = encoded.is_keyframe,
                                    "GDI frame"
                                );
                                crate::service_win::svc_log(&format!(
                                "GDI frame {}: {}x{} keyframe={} bytes={} capture_ms={} encode_ms={}",
                                frame_count,
                                width,
                                height,
                                encoded.is_keyframe,
                                encoded.data.len(),
                                capture_elapsed.as_millis(),
                                encode_elapsed.as_millis()
                            ));
                            }
                            if encoded.is_keyframe {
                                last_keyframe = Instant::now();
                            }
                            if let Err(e) = ipc.send_encoded_frame(&encoded, width, height) {
                                crate::service_win::svc_log(&format!("GDI IPC send failed: {e:#}"));
                                tracing::error!("IPC send failed: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            crate::service_win::svc_log(&format!("GDI encode error: {e:#}"));
                            crate::service_win::svc_log(&format!(
                                "GDI encode error timing: attempt={} capture_ms={} encode_ms={}",
                                gdi_probe_count,
                                capture_elapsed.as_millis(),
                                encode_started.elapsed().as_millis()
                            ));
                            tracing::warn!("GDI encode error: {e}");
                        }
                    }
                }
                Ok(None) => {
                    let capture_elapsed = capture_started.elapsed();
                    if gdi_probe_count <= 3 || capture_elapsed > Duration::from_millis(250) {
                        crate::service_win::svc_log(&format!(
                            "GDI capture returned no frame: attempt={} capture_ms={}",
                            gdi_probe_count,
                            capture_elapsed.as_millis()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    let capture_elapsed = capture_started.elapsed();
                    crate::service_win::svc_log(&format!("GDI capture error: {e:#}"));
                    crate::service_win::svc_log(&format!(
                        "GDI capture error timing: attempt={} capture_ms={}",
                        gdi_probe_count,
                        capture_elapsed.as_millis()
                    ));
                    tracing::warn!("GDI capture error: {e}");
                    gdi_capture = None;
                    cpu_encoder = None;
                }
            }
        } else {
            // No capture available yet — wait for retry
            std::thread::sleep(Duration::from_millis(200));
        }

        // Handle paste text from service (clipboard forwarding)
        if let Some(text) = ipc.take_paste_request() {
            let switched = capture::gdi::switch_to_input_desktop();
            crate::service_win::svc_log(&format!(
                "agent paste: switch_to_input_desktop={switched} desktop={:?}",
                capture::gdi::current_input_desktop_name()
            ));
            if let Some(ref mut inj) = injector {
                tracing::info!(len = text.len(), "agent: pasting text");
                if let Err(e) = inj.type_text(&text) {
                    tracing::warn!("paste failed: {e}");
                }
            }
        }

        // Forward input events
        let raw_inputs = ipc.recv_inputs();
        let raw_input_count = raw_inputs.len();
        let inputs = coalesce_mouse_moves(raw_inputs);
        if !inputs.is_empty() {
            input_events_since_log += raw_input_count;
            tracing::debug!(
                raw = raw_input_count,
                coalesced = inputs.len(),
                "agent received input events"
            );
            if last_input_summary_log.elapsed() > Duration::from_secs(2) {
                crate::service_win::svc_log(&format!(
                    "agent received {} raw input events (coalesced current batch {}->{}) on desktop {:?}",
                    input_events_since_log,
                    raw_input_count,
                    inputs.len(),
                    capture::gdi::current_input_desktop_name()
                ));
                input_events_since_log = 0;
                last_input_summary_log = Instant::now();
            }
        }
        let switched_for_inputs = if inputs.is_empty() {
            false
        } else {
            capture::gdi::switch_to_input_desktop()
        };
        for mut event in inputs {
            let input_kind = match &event {
                InputEvent::MouseMove { .. } => "mouse_move",
                InputEvent::MouseButton { .. } => "mouse_button",
                InputEvent::MouseScroll { .. } => "mouse_scroll",
                InputEvent::Key { .. } => "key",
            };
            // Offset mouse coordinates to the captured display's position
            // on the virtual desktop (needed for secondary displays like VDD).
            if let InputEvent::MouseMove {
                ref mut x,
                ref mut y,
            } = event
            {
                *x += display_x;
                *y += display_y;
                last_mouse_pos = Some((*x, *y));
                let mut log_mouse_move_after_inject = None;
                if last_mouse_move_log.elapsed() > Duration::from_secs(2) {
                    log_mouse_move_after_inject = Some((*x, *y));
                    last_mouse_move_log = Instant::now();
                }
                if let Some(ref mut inj) = injector {
                    if let Err(e) = inj.inject(&event) {
                        crate::service_win::svc_log(&format!("input inject failed: {e:#}"));
                        tracing::warn!("input inject failed: {e}");
                    } else if let Some((mx, my)) = log_mouse_move_after_inject {
                        crate::service_win::svc_log(&format!(
                            "agent input mouse_move: pos=({}, {}) cursor_after={:?} offset=({}, {}) capture={} frame={}x{}",
                            mx,
                            my,
                            crate::input_injector::windows_cursor_diagnostics(),
                            display_x,
                            display_y,
                            capture_mode,
                            width,
                            height
                        ));
                    }
                } else {
                    tracing::warn!("no injector available");
                }
                continue;
            }
            if input_kind != "mouse_move"
                && last_input_detail_log.elapsed() > Duration::from_secs(2)
            {
                let cursor_diag = crate::input_injector::windows_cursor_diagnostics();
                crate::service_win::svc_log(&format!(
                    "agent input {input_kind}: switch_to_input_desktop={switched_for_inputs} desktop={:?} last_mouse={:?} cursor_before={:?} offset=({}, {}) capture={} frame={}x{}",
                    capture::gdi::current_input_desktop_name(),
                    last_mouse_pos,
                    cursor_diag,
                    display_x,
                    display_y,
                    capture_mode,
                    width,
                    height
                ));
                last_input_detail_log = Instant::now();
            }
            if let Some(ref mut inj) = injector {
                if let Err(e) = inj.inject(&event) {
                    crate::service_win::svc_log(&format!("input inject failed: {e:#}"));
                    tracing::warn!("input inject failed: {e}");
                }
            } else {
                tracing::warn!("no injector available");
            }
        }

        if viewer_active && last_cursor_state_sent.elapsed() >= Duration::from_millis(33) {
            if let Some(snapshot) = crate::input_injector::windows_cursor_snapshot() {
                if snapshot.handle != 0
                    && (snapshot.handle != last_cursor_handle || last_cursor_shape.is_none())
                {
                    if let Some(shape) =
                        crate::input_injector::windows_capture_cursor_shape(snapshot.handle)
                    {
                        if last_cursor_shape.as_ref().map(|prev| prev.shape_id)
                            != Some(shape.shape_id)
                        {
                            if let Err(e) = ipc.send_cursor_shape(&shape) {
                                tracing::debug!("IPC cursor shape send failed: {e:#}");
                            } else {
                                tracing::debug!(
                                    handle = snapshot.handle,
                                    shape_id = shape.shape_id,
                                    width = shape.width,
                                    height = shape.height,
                                    hotspot_x = shape.hotspot_x,
                                    hotspot_y = shape.hotspot_y,
                                    "sent cursor shape"
                                );
                                last_cursor_shape = Some(shape);
                                last_cursor_shape_failed_handle = 0;
                            }
                        }
                    } else {
                        if last_cursor_shape_failed_handle != snapshot.handle {
                            tracing::debug!(
                                handle = snapshot.handle,
                                visible = snapshot.visible,
                                x = snapshot.x,
                                y = snapshot.y,
                                "cursor shape capture failed"
                            );
                            last_cursor_shape_failed_handle = snapshot.handle;
                        }
                        last_cursor_shape = None;
                    }
                    last_cursor_handle = snapshot.handle;
                }

                let local_x = snapshot.x.saturating_sub(display_x);
                let local_y = snapshot.y.saturating_sub(display_y);
                let max_x = width.saturating_sub(1) as i32;
                let max_y = height.saturating_sub(1) as i32;
                let visible = width > 0
                    && height > 0
                    && snapshot.visible
                    && local_x >= 0
                    && local_y >= 0
                    && local_x <= max_x
                    && local_y <= max_y;
                let state = CursorState {
                    visible,
                    x: local_x.clamp(0, max_x),
                    y: local_y.clamp(0, max_y),
                    shape_id: last_cursor_shape
                        .as_ref()
                        .map(|shape| shape.shape_id)
                        .unwrap_or(0),
                };
                if last_cursor_state != Some(state)
                    || last_cursor_state_sent.elapsed() >= Duration::from_millis(250)
                {
                    if let Err(e) = ipc.send_cursor_state(&state) {
                        tracing::debug!("IPC cursor state send failed: {e:#}");
                    } else {
                        last_cursor_state = Some(state);
                        last_cursor_state_sent = Instant::now();
                    }
                }
            }
        }

        // Poll clipboard for changes (send to service → client)
        if clipboard_poll.elapsed() > Duration::from_millis(500) {
            clipboard_poll = Instant::now();
            if let Some(ref mut ab) = arboard {
                if let Ok(text) = ab.get_text() {
                    if !text.is_empty() && text != last_clipboard {
                        last_clipboard = text.clone();
                        let _ = ipc.send_clipboard(&text);
                    }
                }
            }
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    tracing::info!("Agent shutting down");
    Ok(())
}

fn print_connection_code(addr: &str) {
    let code = format!("phantom://{addr}");
    let cmd = format!("phantom-client -c {addr}");
    let note = "Ensure port forwarding is configured on your router.";
    let w = code.len().max(cmd.len()).max(note.len()) + 4;
    let bar = "═".repeat(w + 2);
    println!("\n╔{bar}╗");
    println!("║  {code:<w$}  ║");
    println!("║  {:<w$}  ║", "");
    println!("║  {cmd:<w$}  ║");
    println!("║  {:<w$}  ║", "");
    println!("║  {note:<w$}  ║");
    println!("╚{bar}╝\n");
}
