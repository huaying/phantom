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

use anyhow::Result;
use clap::Parser;
use phantom_core::crypto;
use phantom_core::encode::{FrameEncoder, VideoCodec};
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

    /// Install as auto-start. Windows: registers the Phantom Windows Service
    /// (LocalSystem, auto start) and installs the Virtual Display Driver.
    /// Linux: writes a systemd --user unit and enables it. The blessed Linux
    /// path for end users is the XDG autostart entry written by install.sh;
    /// use --install only when running manually outside of that flow.
    #[arg(long)]
    install: bool,

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
            return run_agent_mode(args.ipc_session);
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

    if args.install {
        return install_autostart();
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

    // Hardware probe: resolve "auto" encoder and capture
    let gpu_probe = phantom_gpu::probe::probe();
    // `mut` used only on linux/windows fallback paths
    #[allow(unused_mut)]
    let mut encoder_name = if args.encoder == "auto" {
        gpu_probe.best_encoder().to_string()
    } else {
        args.encoder.clone()
    };
    let mut capture_name = if args.capture == "auto" {
        // On Wayland sessions, prefer PipeWire capture (if feature enabled)
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
    // If encoder is explicitly non-GPU but capture resolved to a GPU-only method, fix it
    if encoder_name == "openh264" && (capture_name == "nvfbc" || capture_name == "dxgi") {
        tracing::info!(
            "encoder is openh264, overriding capture from {} to scrap",
            capture_name
        );
        capture_name = "scrap".to_string();
    }

    // Default codec selection intentionally picks H.264 even when the
    // GPU can do AV1. Reason: AV1 support on the client side is the
    // weak link — software dav1d on a mid-range Mac / Intel Chrome can
    // cost 20-40 ms per 1080p frame, which shows up as typing lag on
    // the native client and as outright browser-tab OOM crashes on the
    // web client (observed on U22 L40). H.264 decodes on every
    // platform we ship with hardware acceleration (VideoToolbox on
    // macOS, NVDEC on Linux/Windows, WebCodecs H.264 everywhere).
    //
    // AV1 is kept as an explicit opt-in (`--codec av1`) while we:
    //   (a) teach ClientHello to advertise supported decoders
    //   (b) let the server pick the codec intersection
    // Until (a) + (b) land, defaulting to AV1 is a regression for the
    // majority of clients even when the server supports it.
    let video_codec = match args.codec.as_str() {
        "auto" => {
            tracing::info!(
                "codec=auto → H.264 (AV1 is opt-in via --codec av1; client decode support still rolling out)"
            );
            VideoCodec::H264
        }
        "h264" | "H264" | "h.264" => VideoCodec::H264,
        "av1" | "AV1" => {
            if encoder_name != "nvenc" {
                anyhow::bail!("AV1 codec requires --encoder nvenc (OpenH264 only supports H.264)");
            }
            // Non-fatal probe: warn if the GPU isn't reporting AV1 but
            // let it through — the NVENC init will surface the real
            // error in a consistent format if it really can't.
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
    let pending: Arc<std::sync::Mutex<Option<ConnectionPair>>> =
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
                    Ok((sender, mut receiver)) => {
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
                            drop(sender);
                            drop(receiver);
                            continue;
                        }

                        // Replace any previously queued (but not yet consumed) connection
                        *pending.lock().unwrap() = Some((sender, receiver));
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

        let (sender, receiver) = match conn {
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

        // Post-session cleanup
        differ.reset();
        if let Some(ref mut enc) = video_encoder {
            enc.force_keyframe();
        }
        #[cfg(target_os = "linux")]
        if let Some(ref mut gpu) = gpu {
            let _ = gpu.capture.release_context();
            if let Err(e) = gpu.reset_for_new_session() {
                tracing::error!("GPU pipeline reset failed: {e}");
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

// ── GPU pipeline struct (Linux) ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct GpuPipeline {
    capture: phantom_gpu::nvfbc::NvfbcCapture,
    encoder: phantom_gpu::nvenc::NvencEncoder,
    cuda: std::sync::Arc<phantom_gpu::cuda::CudaLib>,
    ctx: phantom_gpu::sys::CUcontext,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    codec: VideoCodec,
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

        let first = loop {
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
            cuda,
            ctx: primary_ctx,
            width,
            height,
            fps,
            bitrate: bitrate_kbps,
            codec,
        })
    }

    fn reset_for_new_session(&mut self) -> Result<()> {
        self.capture.reset_session()?;
        self.encoder = unsafe {
            phantom_gpu::nvenc::NvencEncoder::with_context(
                std::sync::Arc::clone(&self.cuda),
                self.ctx,
                false,
                self.width,
                self.height,
                self.fps,
                self.bitrate,
                self.codec,
            )?
        };
        tracing::info!("GPU pipeline reset for new session");
        Ok(())
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

fn install_autostart() -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("get current exe path")?;
    #[allow(unused_variables)]
    let exe_str = exe.to_string_lossy();

    #[cfg(target_os = "windows")]
    return service_win::install_service();

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
fn run_agent_mode(ipc_session: Option<u32>) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Agent has no console (spawned by the service). Set up tracing to write
    // to a log file in the system temp directory instead of stdout.
    let log_file = std::env::temp_dir().join("phantom-agent.log");
    if let Ok(file) = std::fs::File::create(&log_file) {
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
    let ipc = match ipc_pipe::IpcClient::connect(ipc_session) {
        Ok(c) => {
            tracing::info!("IPC connected");
            c
        }
        Err(e) => {
            tracing::error!("IPC connect FAILED: {e}");
            return Err(e);
        }
    };

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
    run_agent_loop(&ipc, &mut injector, &shutdown)
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
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mut device_idx = 0u32;
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
            if desc == "Virtual Display Driver" {
                tracing::info!(name, desc, "Found VDD device");
                return Some(name);
            }
            device_idx += 1;
        }
        tracing::warn!("VDD device not found");
        None
    }
}

/// Change the display resolution using ChangeDisplaySettingsExW.
/// Targets the VDD virtual display (highest-res non-primary monitor).
/// Same approach as Sunshine: find the right display device and change its settings.
#[cfg(target_os = "windows")]
fn change_display_resolution(width: u32, height: u32) -> bool {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        // Enumerate display devices to find VDD
        let mut device_idx = 0u32;
        let mut target_device: Option<String> = None;

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
            tracing::info!(device_idx, name, desc, "Display device");

            // Match MttVDD (Virtual Display Driver by MiketheTech) specifically.
            // "Virtual Display Driver" is the exact DeviceString for VDD.
            // Do NOT match "AWS Indirect Display Device" (DCV) or other IDD drivers.
            if desc == "Virtual Display Driver" {
                target_device = Some(name);
                break;
            }
            device_idx += 1;
        }

        let device_name = match target_device {
            Some(name) => name,
            None => {
                tracing::warn!("No VDD device found for resolution change");
                return false;
            }
        };

        // Get current settings
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

        // Set new resolution
        dm.dmPelsWidth = width;
        dm.dmPelsHeight = height;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;

        let result = ChangeDisplaySettingsExW(
            pcwstr,
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        );

        if result == DISP_CHANGE_SUCCESSFUL {
            // Apply the change
            let _ = ChangeDisplaySettingsExW(None, None, None, CDS_TYPE(0), None);
            tracing::info!(
                width,
                height,
                device = device_name,
                "Display resolution changed"
            );
            true
        } else {
            tracing::warn!(?result, width, height, "ChangeDisplaySettingsExW failed");
            false
        }
    }
}

/// Make the MTT VDD the primary display so all windows open on it.
///
/// On VMs where another display (e.g. NVIDIA L40 with a CLB2770 EDID emulator,
/// or DCV's IDD with a connected client) is primary, new windows open there
/// and our VDD stays empty → Phantom captures a black screen. Setting VDD
/// primary forces new windows onto VDD; existing windows can be moved by user.
///
/// Same approach as Sunshine: position VDD at (0,0) and set CDS_SET_PRIMARY.
/// Windows automatically repositions other displays to negative coordinates.
/// Idempotent — calling on an already-primary VDD is a no-op.
/// Deprecated — the legacy `ChangeDisplaySettingsExW(CDS_SET_PRIMARY)` path.
/// Windows 11 24H2+ returns `DISP_CHANGE_FAILED` for IDD drivers. Kept around
/// but unused; real work is in `display_ccd::set_vdd_primary`.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn set_vdd_primary_legacy(vdd_device: &str) -> bool {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        // Step 1: enumerate all active displays. Get VDD's current width to know
        // where to place other displays.
        let mut vdd_w: u32 = 0;
        let mut others: Vec<(String, i32, i32, u32, u32)> = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut dd = DISPLAY_DEVICEW::default();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(None, idx, &mut dd, 0).as_bool() {
                break;
            }
            idx += 1;
            // Only attached (active) displays matter
            // DISPLAY_DEVICE_ATTACHED_TO_DESKTOP = 0x1
            if (dd.StateFlags & 0x1) == 0 {
                continue;
            }
            let name = String::from_utf16_lossy(
                &dd.DeviceName[..dd.DeviceName.iter().position(|&c| c == 0).unwrap_or(32)],
            );
            let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut dm = DEVMODEW::default();
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if !EnumDisplaySettingsW(
                windows::core::PCWSTR(name_w.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dm,
            )
            .as_bool()
            {
                continue;
            }
            let x = dm.Anonymous1.Anonymous2.dmPosition.x;
            let y = dm.Anonymous1.Anonymous2.dmPosition.y;
            if name == vdd_device {
                vdd_w = dm.dmPelsWidth;
            } else {
                others.push((name, x, y, dm.dmPelsWidth, dm.dmPelsHeight));
            }
        }
        if vdd_w == 0 {
            crate::service_win::svc_log(&format!(
                "set_vdd_primary: VDD {vdd_device} not found among attached displays"
            ));
            return false;
        }

        // Step 2: queue moves for all non-VDD displays. Place them sequentially
        // to the right of VDD: first at x=vdd_w, next at vdd_w+w1, etc.
        let mut x_offset = vdd_w as i32;
        for (name, ox, oy, w, h) in &others {
            let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let pc = windows::core::PCWSTR(name_w.as_ptr());
            let mut dm = DEVMODEW::default();
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if !EnumDisplaySettingsW(pc, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
                continue;
            }
            dm.Anonymous1.Anonymous2.dmPosition.x = x_offset;
            dm.Anonymous1.Anonymous2.dmPosition.y = 0;
            dm.dmFields = DM_POSITION;
            let r = ChangeDisplaySettingsExW(
                pc,
                Some(&dm),
                None,
                CDS_UPDATEREGISTRY | CDS_NORESET,
                None,
            );
            crate::service_win::svc_log(&format!(
                "set_vdd_primary: queue move {name} ({ox},{oy})->({x_offset},0) size={w}x{h} = {}",
                r.0
            ));
            x_offset += *w as i32;
        }

        // Step 3: queue VDD at (0,0) as primary.
        let vdd_w_str: Vec<u16> = vdd_device
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let vdd_pc = windows::core::PCWSTR(vdd_w_str.as_ptr());
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if !EnumDisplaySettingsW(vdd_pc, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            crate::service_win::svc_log(&format!(
                "set_vdd_primary({vdd_device}): EnumDisplaySettingsW failed"
            ));
            return false;
        }
        dm.Anonymous1.Anonymous2.dmPosition.x = 0;
        dm.Anonymous1.Anonymous2.dmPosition.y = 0;
        dm.dmFields = DM_POSITION;
        let r = ChangeDisplaySettingsExW(
            vdd_pc,
            Some(&dm),
            None,
            CDS_SET_PRIMARY | CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        );
        crate::service_win::svc_log(&format!(
            "set_vdd_primary({vdd_device}): queue primary = {}",
            r.0
        ));
        if r != DISP_CHANGE_SUCCESSFUL {
            return false;
        }

        // Step 4: apply all queued changes atomically.
        let apply = ChangeDisplaySettingsExW(None, None, None, CDS_TYPE(0), None);
        crate::service_win::svc_log(&format!(
            "set_vdd_primary({vdd_device}): apply = {}",
            apply.0
        ));
        apply == DISP_CHANGE_SUCCESSFUL
    }
}

/// Agent capture+encode loop following RustDesk/Sunshine pattern:
/// - Calls OpenInputDesktop + SetThreadDesktop before capture (follows desktop switches)
/// - On DXGI error: reinit pipeline (don't crash)
/// - Survives lock/unlock transitions
#[cfg(target_os = "windows")]
fn activate_gdi_fallback(
    cpu_encoder: &mut Option<Box<dyn phantom_core::encode::FrameEncoder>>,
    gdi_capture: &mut Option<capture::gdi::GdiCapture>,
    width: &mut u32,
    height: &mut u32,
    display_x: &mut i32,
    display_y: &mut i32,
    capture_mode: &mut &'static str,
) {
    match capture::gdi::GdiCapture::new() {
        Ok(gdi) => {
            let (w, h) = phantom_core::capture::FrameCapture::resolution(&gdi);
            *width = w;
            *height = h;
            *display_x = 0;
            *display_y = 0;
            match encode::h264::OpenH264Encoder::new(w, h, 15.0, 2000) {
                Ok(mut enc) => {
                    enc.force_keyframe();
                    crate::service_win::svc_log(&format!("Tier 3: GDI+OpenH264 {}x{}", *width, *height));
                    *cpu_encoder = Some(Box::new(enc));
                    *gdi_capture = Some(gdi);
                    *capture_mode = "gdi_h264";
                }
                Err(e) => tracing::warn!("OpenH264 init failed: {e}"),
            }
        }
        Err(e) => tracing::warn!("GDI fallback init failed: {e}"),
    }
}

#[cfg(target_os = "windows")]
fn run_agent_loop(
    ipc: &ipc_pipe::IpcClient,
    injector: &mut Option<input_injector::InputInjector>,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use phantom_core::capture::FrameCapture;
    use phantom_core::encode::FrameEncoder;
    use phantom_core::input::InputEvent;

    let frame_interval = Duration::from_secs_f64(1.0 / 30.0);

    // Clipboard monitoring (agent → service → client)
    let mut arboard = arboard::Clipboard::new().ok();
    let mut last_clipboard = String::new();
    let mut clipboard_poll = Instant::now();

    // Attach to input desktop BEFORE calling SetDisplayConfig. CCD API returns
    // ERROR_ACCESS_DENIED if the calling thread isn't attached to an interactive
    // desktop. Same reason capture calls need this.
    capture::gdi::switch_to_input_desktop();

    // Find VDD device name (e.g. \\.\DISPLAY10). We only make it primary
    // after Tier 1 DXGI→NVENC is confirmed working; otherwise a machine with
    // no usable NVIDIA stack can get the desktop moved onto VDD while the CPU
    // fallback keeps capturing a different adapter, producing a black stream.
    let vdd_device = find_vdd_device_name();
    crate::service_win::svc_log(&format!("agent: vdd_device = {:?}", vdd_device));

    tracing::info!("Deferring VDD primary switch until Tier 1 is confirmed");
    // Log displays before any topology change so we can diagnose which adapter
    // Tier 2 ends up targeting.
    if let Ok(displays) = capture::scrap::ScrapCapture::list_displays() {
        for d in &displays {
            crate::service_win::svc_log(&format!(
                "agent: display[{}] {}x{} primary={}",
                d.index, d.width, d.height, d.is_primary
            ));
        }
    }

    // Guard to restore topology on graceful shutdown. Drop-based restore so it
    // fires even if the agent loop panics. If the process is killed outright
    // (SIGKILL / TerminateProcess), reboot still restores since we didn't
    // save to the display database.
    struct TopologyGuard(Option<display_ccd::Topology>);
    impl Drop for TopologyGuard {
        fn drop(&mut self) {
            if let Some(topo) = self.0.take() {
                match display_ccd::restore(&topo) {
                    Ok(()) => crate::service_win::svc_log("agent: topology restored"),
                    Err(e) => crate::service_win::svc_log(&format!("agent: restore failed: {e:#}")),
                }
            }
        }
    }
    let mut topology_guard = TopologyGuard(None);
    let mut vdd_primary_active = false;

    // Capture tiers (best to worst):
    // 1. DXGI(VDD)→NVENC zero-copy (GPU capture + GPU encode, ~4ms)
    // 2. DXGI(VDD)→CPU encode     (any platform with VDD)
    // 3. GDI→CPU encode            (lock screen fallback)
    let mut gpu_pipeline: Option<phantom_gpu::dxgi_nvenc::DxgiNvencPipeline> = None;
    let mut scrap_capture: Option<capture::scrap::ScrapCapture> = None;
    let mut gdi_capture: Option<capture::gdi::GdiCapture> = None;
    let mut cpu_encoder: Option<Box<dyn FrameEncoder>> = None;

    let mut frame_count = 0u64;
    let mut last_keyframe = Instant::now();
    let mut last_init_attempt = Instant::now() - Duration::from_secs(10);
    let mut width = 1920u32;
    let mut height = 1080u32;
    let mut capture_mode = "none";
    let mut scrap_waiting_for_frame_since: Option<Instant> = None;
    // Display offset on virtual desktop — needed to map mouse coordinates
    // when capturing from a secondary display (e.g. VDD).
    let mut display_x: i32 = 0;
    let mut display_y: i32 = 0;
    // Track input desktop name (e.g. "Default" vs "Winlogon") so we can reset
    // the DXGI duplication when Windows switches desktops on lock / unlock /
    // UAC prompts. Duplication objects are scoped to the desktop they were
    // created on and keep returning stale frames from the now-hidden desktop.
    let mut last_desktop_name: Option<String> = None;

    tracing::info!("Starting agent loop");

    while !shutdown.load(Ordering::Relaxed) && !ipc.should_shutdown() {
        let loop_start = Instant::now();
        capture::gdi::switch_to_input_desktop();
        // Detect desktop switch (Winlogon ↔ Default): if changed, force reset
        // of all capture pipelines so the new duplication targets the right
        // desktop. Without this, after lock→unlock the client keeps seeing
        // the login screen until the user manually reconnects.
        let cur_desktop = capture::gdi::current_input_desktop_name();
        if cur_desktop != last_desktop_name {
            if last_desktop_name.is_some() {
                crate::service_win::svc_log(&format!(
                    "Input desktop changed: {:?} → {:?} — resetting capture",
                    last_desktop_name, cur_desktop
                ));
                gpu_pipeline = None;
                scrap_capture = None;
                gdi_capture = None;
                cpu_encoder = None;
                last_init_attempt = Instant::now() - Duration::from_secs(10);
            }
            last_desktop_name = cur_desktop;
        }

        // Try to init/reinit capture pipeline (best available)
        if gpu_pipeline.is_none()
            && scrap_capture.is_none()
            && gdi_capture.is_none()
            && last_init_attempt.elapsed() > Duration::from_secs(1)
        {
            last_init_attempt = Instant::now();

            // Tier 1: DXGI(VDD)→NVENC zero-copy
            match phantom_gpu::dxgi_nvenc::DxgiNvencPipeline::with_target_device(
                30,
                5000,
                vdd_device.as_deref(),
            ) {
                Ok(mut gpu) => {
                    if !vdd_primary_active {
                        if let Some(ref dev) = vdd_device {
                            match display_ccd::set_vdd_primary(dev) {
                                Ok(topo) => {
                                    crate::service_win::svc_log(&format!(
                                        "agent: set_vdd_primary({dev}) OK — saved {} paths, {} modes",
                                        topo.paths.len(),
                                        topo.modes.len()
                                    ));
                                    topology_guard.0 = Some(topo);
                                    vdd_primary_active = true;
                                }
                                Err(e) => {
                                    crate::service_win::svc_log(&format!(
                                        "agent: set_vdd_primary({dev}) failed after Tier 1 init: {e:#}"
                                    ));
                                }
                            }
                        }
                    }
                    width = gpu.width;
                    height = gpu.height;
                    gpu.force_keyframe();
                    crate::service_win::svc_log(&format!(
                        "Tier 1: DXGI→NVENC ready {}x{}",
                        width, height
                    ));
                    gpu_pipeline = Some(gpu);
                    scrap_capture = None;
                    gdi_capture = None;
                    cpu_encoder = None;
                    scrap_waiting_for_frame_since = None;
                    capture_mode = "dxgi_nvenc";
                }
                Err(e) => {
                    crate::service_win::svc_log(&format!("Tier 1 DXGI→NVENC unavailable: {e:#}"));

                    // Tier 2: ScrapCapture (DXGI + CPU readback) — follow the
                    // existing primary display instead of forcing VDD.
                    let displays =
                        capture::scrap::ScrapCapture::list_displays().unwrap_or_default();
                    let best_idx = displays
                        .iter()
                        .find(|d| d.is_primary)
                        .or_else(|| displays.first())
                        .map(|d| d.index)
                        .unwrap_or(0);
                    tracing::info!(
                        best_idx,
                        displays = ?displays.iter().map(|d| format!("{}:{}x{}", d.index, d.width, d.height)).collect::<Vec<_>>(),
                        "ScrapCapture fallback display selection"
                    );
                    match capture::scrap::ScrapCapture::with_display(best_idx) {
                        Ok(scrap) => {
                            let (w, h) = scrap.resolution();
                            width = w;
                            height = h;
                            // Get display origin on virtual desktop for mouse offset
                            let (dx, dy) = get_display_origin(w, h, scrap.display_index());
                            display_x = dx;
                            display_y = dy;
                            match encode::h264::OpenH264Encoder::new(width, height, 30.0, 5000) {
                                Ok(mut enc) => {
                                    enc.force_keyframe();
                                    crate::service_win::svc_log(&format!(
                                        "Tier 2: ScrapCapture+OpenH264 {}x{} (CPU path)",
                                        width, height
                                    ));
                                    scrap_capture = Some(scrap);
                                    cpu_encoder = Some(Box::new(enc));
                                    scrap_waiting_for_frame_since = Some(Instant::now());
                                    capture_mode = "scrap_h264";
                                }
                                Err(e) => tracing::warn!("OpenH264 init failed: {e}"),
                            }
                        }
                        Err(e) => {
                            tracing::debug!("ScrapCapture unavailable: {e}");

                            // Tier 3: GDI + OpenH264 (lock screen fallback)
                            activate_gdi_fallback(
                                &mut cpu_encoder,
                                &mut gdi_capture,
                                &mut width,
                                &mut height,
                                &mut display_x,
                                &mut display_y,
                                &mut capture_mode,
                            );
                            if gdi_capture.is_some() {
                                tracing::info!(
                                    width,
                                    height,
                                    "Tier 3: GDI+OpenH264 fallback"
                                );
                                scrap_waiting_for_frame_since = None;
                            } else if frame_count == 0 {
                                tracing::warn!("All capture methods failed");
                            }
                        }
                    }
                }
            }
        }

        // Handle resolution change requests (adaptive resolution like DCV/Sunshine)
        if let Some((new_w, new_h)) = ipc.take_resolution_request() {
            if new_w != width || new_h != height {
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
                scrap_capture = None;
                gdi_capture = None;
                cpu_encoder = None;
                scrap_waiting_for_frame_since = None;

                if change_display_resolution(new_w, new_h) {
                    // change_display_resolution uses legacy ChangeDisplaySettingsExW
                    // with CDS_UPDATEREGISTRY, which can re-activate detached paths
                    // (NVIDIA comes back as primary). Only re-apply CCD if the
                    // exclusive state actually got broken — the query is ~10ms,
                    // SetDisplayConfig is ~100-200ms.
                    if vdd_primary_active {
                        if let Some(ref dev) = vdd_device {
                            if !display_ccd::is_vdd_primary(dev) {
                                match display_ccd::set_vdd_primary(dev) {
                                    Ok(_) => crate::service_win::svc_log(
                                        "agent: re-applied CCD after resize (was broken)",
                                    ),
                                    Err(e) => crate::service_win::svc_log(&format!(
                                        "agent: re-apply CCD failed after resize: {e:#}"
                                    )),
                                }
                            }
                        }
                    }
                    last_init_attempt = Instant::now() - Duration::from_secs(10);
                    // Brief pause for Windows to settle. 200ms is enough for
                    // DXGI/GDI to reflect the new mode; 500ms was overly safe.
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }

        // Handle keyframe requests from service (new session).
        // Uses capture reset to force DXGI to return a frame on static desktop.
        if ipc.take_keyframe_request() {
            tracing::info!("Agent: received keyframe request from service");
            // Keyframe request fires on new client session. Re-apply CCD
            // exclusive only if state got broken — saves ~150ms per session
            // start on the common case.
            if vdd_primary_active {
                if let Some(ref dev) = vdd_device {
                    if !display_ccd::is_vdd_primary(dev) {
                        let _ = display_ccd::set_vdd_primary(dev);
                    }
                }
            }
            if let Some(ref mut gpu) = gpu_pipeline {
                gpu.force_keyframe_with_capture_reset();
            }
            if let Some(ref mut scrap) = scrap_capture {
                // Reset ScrapCapture so DXGI returns a frame on static desktop
                let _ = scrap.reset();
                scrap_waiting_for_frame_since = Some(Instant::now());
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

        // Capture + encode: Tier 1 — GPU path (DXGI→NVENC zero-copy)
        if let Some(ref mut gpu) = gpu_pipeline {
            match gpu.capture_and_encode() {
                Ok(Some(encoded)) => {
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
                }
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    tracing::warn!("DXGI→NVENC error: {e:#}, falling back");
                    gpu_pipeline = None;
                    // Cooldown before retrying Tier 1 — let Tier 2/3 take over.
                    // Without this, ACCESS_LOST on lock screen causes infinite
                    // Tier 1 init→fail loop that never reaches Tier 2/3.
                    last_init_attempt = Instant::now();
                }
            }
        }
        // Capture + encode: Tier 2 — ScrapCapture (DXGI CPU) + OpenH264
        else if scrap_capture.is_some() && cpu_encoder.is_some() {
            match scrap_capture.as_mut().expect("scrap_capture checked").capture() {
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
                    Err(e) => tracing::warn!("ScrapCapture encode error: {e}"),
                },
                Ok(None) => {
                    if scrap_waiting_for_frame_since
                        .is_some_and(|since| since.elapsed() > Duration::from_millis(1500))
                    {
                        tracing::warn!(
                            width,
                            height,
                            "ScrapCapture produced no frame after reset/init, switching to GDI"
                        );
                        crate::service_win::svc_log(
                            "ScrapCapture stalled after reset/init; switching to GDI",
                        );
                        scrap_capture = None;
                        cpu_encoder = None;
                        scrap_waiting_for_frame_since = None;
                        activate_gdi_fallback(
                            &mut cpu_encoder,
                            &mut gdi_capture,
                            &mut width,
                            &mut height,
                            &mut display_x,
                            &mut display_y,
                            &mut capture_mode,
                        );
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    tracing::warn!("ScrapCapture error: {e}, switching to GDI");
                    scrap_capture = None;
                    cpu_encoder = None;
                    scrap_waiting_for_frame_since = None;
                    activate_gdi_fallback(
                        &mut cpu_encoder,
                        &mut gdi_capture,
                        &mut width,
                        &mut height,
                        &mut display_x,
                        &mut display_y,
                        &mut capture_mode,
                    );
                }
            }
        }
        // Capture + encode: Tier 3 — GDI fallback (lock screen, no display)
        else if let (Some(ref mut gdi), Some(ref mut enc)) = (&mut gdi_capture, &mut cpu_encoder)
        {
            match gdi.capture() {
                Ok(Some(frame)) => match enc.encode_frame(&frame) {
                    Ok(encoded) => {
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
                        }
                        if encoded.is_keyframe {
                            last_keyframe = Instant::now();
                        }
                        if let Err(e) = ipc.send_encoded_frame(&encoded, width, height) {
                            tracing::error!("IPC send failed: {e}");
                            break;
                        }
                    }
                    Err(e) => tracing::warn!("GDI encode error: {e}"),
                },
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
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
            if let Some(ref mut inj) = injector {
                tracing::info!(len = text.len(), "agent: pasting text");
                if let Err(e) = inj.type_text(&text) {
                    tracing::warn!("paste failed: {e}");
                }
            }
        }

        // Forward input events
        let inputs = ipc.recv_inputs();
        if !inputs.is_empty() {
            tracing::info!(count = inputs.len(), "agent received input events");
        }
        for mut event in inputs {
            capture::gdi::switch_to_input_desktop();
            // Offset mouse coordinates to the captured display's position
            // on the virtual desktop (needed for secondary displays like VDD).
            if let InputEvent::MouseMove {
                ref mut x,
                ref mut y,
            } = event
            {
                *x += display_x;
                *y += display_y;
            }
            if let Some(ref mut inj) = injector {
                if let Err(e) = inj.inject(&event) {
                    tracing::warn!("input inject failed: {e}");
                }
            } else {
                tracing::warn!("no injector available");
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
