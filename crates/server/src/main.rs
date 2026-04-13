//! Phantom remote desktop server.
//!
//! Captures the screen (via scrap, NVFBC, PipeWire, or DXGI), encodes it
//! (OpenH264 or NVENC), and streams to connected clients over TCP, QUIC,
//! WebSocket, or WebRTC. Supports encrypted connections, audio capture,
//! bidirectional file transfer, and clipboard synchronization.

#[cfg(feature = "audio")]
mod audio_capture;
#[cfg(feature = "wayland")]
mod capture_pipewire;
mod capture_scrap;
mod encode_h264;
mod encode_zstd;
mod file_transfer;
mod input_injector;
mod session;
mod transport_quic;
mod transport_tcp;
#[cfg(feature = "webrtc")]
mod transport_webrtc;
mod transport_ws;

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
#[command(name = "phantom-server", about = "Phantom remote desktop server")]
struct Args {
    #[arg(short, long, default_value = "0.0.0.0:9900")]
    listen: String,
    #[arg(short, long, default_value_t = 30)]
    fps: u32,
    #[arg(short, long, default_value_t = 5000)]
    bitrate: u32,
    #[arg(long, default_value_t = 2000)]
    quality_delay_ms: u64,
    #[arg(short, long)]
    key: Option<String>,
    #[arg(long)]
    no_encrypt: bool,

    /// Video encoder: auto (default, probes GPU), openh264 (CPU), nvenc (NVIDIA GPU).
    #[arg(long, default_value = "auto")]
    encoder: String,

    /// Video codec: auto (default, uses AV1 if GPU supports it), h264, av1.
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

    /// Install as auto-start (Windows: logon task, Linux: systemd service).
    #[arg(long)]
    install: bool,

    /// Remove auto-start registration.
    #[arg(long)]
    uninstall: bool,

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
}

type ConnectionPair = (Box<dyn MessageSender>, Box<dyn MessageReceiver>);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("phantom=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

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
        match capture_scrap::ScrapCapture::list_displays() {
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

    let frame_interval = Duration::from_secs_f64(1.0 / args.fps as f64);
    let quality_delay = Duration::from_millis(args.quality_delay_ms);

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

    let video_codec = match args.codec.as_str() {
        "auto" => {
            let codec_name = gpu_probe.best_codec();
            tracing::info!(codec = codec_name, "auto-detected video codec");
            match codec_name {
                "av1" => VideoCodec::Av1,
                _ => VideoCodec::H264,
            }
        }
        "h264" | "H264" | "h.264" => VideoCodec::H264,
        "av1" | "AV1" => {
            if encoder_name != "nvenc" {
                anyhow::bail!("AV1 codec requires --encoder nvenc (OpenH264 only supports H.264)");
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

    for transport in &transports {
        match *transport {
            "tcp" => {
                let tcp_addr = format!("{listen_host}:{base_port}");
                let tcp_listener = transport_tcp::TcpServerTransport::bind(&tcp_addr)?;
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
                let mut ws_transport =
                    transport_ws::WebServerTransport::start(web_port, web_port + 1, web_port + 2)?;
                tracing::info!("open https://localhost:{web_port} in browser");
                // Extract audio WS receiver before moving transport into accept thread
                let audio_ws_rx = ws_transport.take_audio_ws_rx();
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
                let quic_listener = transport_quic::QuicServerTransport::bind(&quic_addr)?;
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

    {
        let conn_rx = Arc::clone(&conn_rx);
        let pending = Arc::clone(&pending);
        let cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("doorbell".into())
            .spawn(move || loop {
                let pair = { conn_rx.lock().unwrap().recv() };
                match pair {
                    Ok(conn) => {
                        // Replace any previously queued (but not yet consumed) connection
                        *pending.lock().unwrap() = Some(conn);
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
                    quality_delay,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    audio_ws_rx: None,
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
                    quality_delay,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    audio_ws_rx: None,
                },
            )
        };
        #[cfg(target_os = "linux")]
        {
            _active_session_token = result.session_token.clone();
            tracing::info!("session ended: {}", result.error);
        }
        #[cfg(target_os = "windows")]
        let result = if let Some(ref mut gw) = gpu_win {
            session::run_session_dxgi(
                gw,
                session::SessionConfig {
                    sender,
                    receiver,
                    frame_interval,
                    quality_delay,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    audio_ws_rx: None,
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
                    quality_delay,
                    cancel: session_cancel,
                    send_file: send_file_path.as_deref(),
                    video_codec,
                    is_resume,
                    audio_ws_rx: None,
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
                quality_delay,
                cancel: session_cancel,
                send_file: send_file_path.as_deref(),
                video_codec,
                is_resume,
                    audio_ws_rx: None,
            },
        );

        // Update active session token from session result
        #[cfg(not(target_os = "linux"))]
        {
            _active_session_token = result.session_token.clone();
            tracing::info!("session ended: {}", result.error);
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
            let cap = capture_scrap::ScrapCapture::with_display(display_index)?;
            Ok(Box::new(cap))
        }
        #[cfg(feature = "wayland")]
        "pipewire" => {
            if display_index != 0 {
                tracing::warn!(
                    "PipeWire capture: --display is ignored (portal handles display selection)"
                );
            }
            let cap = capture_pipewire::PipeWireCapture::new()?;
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
            let enc = encode_h264::OpenH264Encoder::new(width, height, fps, bitrate_kbps)?;
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

// ── Auto-start install/uninstall ────────────────────────────────────────────

fn install_autostart() -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("get current exe path")?;
    #[allow(unused_variables)]
    let exe_str = exe.to_string_lossy();

    #[cfg(target_os = "windows")]
    {
        let status = std::process::Command::new("schtasks")
            .args([
                "/Create",
                "/TN",
                "PhantomServer",
                "/TR",
                &format!("\"{exe_str}\" --no-encrypt --transport web"),
                "/SC",
                "ONLOGON",
                "/RL",
                "HIGHEST",
                "/IT",
                "/F",
            ])
            .status()
            .context("schtasks")?;
        if status.success() {
            println!("Installed: PhantomServer scheduled task (runs at logon)");
            println!("  To start now: schtasks /Run /TN PhantomServer");
            println!("  To remove:    phantom-server --uninstall");
        } else {
            anyhow::bail!("schtasks failed with {status}");
        }
    }

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
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        println!("Auto-start not yet supported on this OS. Run phantom-server manually.");
    }

    Ok(())
}

fn uninstall_autostart() -> Result<()> {
    #[allow(unused_imports)]
    use anyhow::Context;
    #[cfg(target_os = "windows")]
    {
        let status = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", "PhantomServer", "/F"])
            .status()
            .context("schtasks delete")?;
        if status.success() {
            println!("Removed: PhantomServer scheduled task");
        } else {
            anyhow::bail!("schtasks delete failed");
        }
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "phantom-server"])
            .status()?;
        let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".config/systemd/user/phantom-server.service");
        let _ = std::fs::remove_file(&path);
        println!("Removed: systemd user service");
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        println!("Auto-start not supported on this OS.");
    }

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
