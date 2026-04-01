mod capture_scrap;
mod encode_h264;
mod encode_zstd;
mod input_injector;
mod transport_quic;
mod transport_tcp;
mod transport_webrtc;
mod transport_ws;

use anyhow::Result;
use clap::Parser;
use phantom_core::capture::FrameCapture;
use phantom_core::clipboard::ClipboardTracker;
use phantom_core::crypto;
use phantom_core::encode::{Encoder, FrameEncoder};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::input::InputEvent;
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::mpsc;
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

    /// Video encoder: openh264 (CPU, default), nvenc (NVIDIA GPU).
    #[arg(long, default_value = "openh264")]
    encoder: String,

    /// Screen capture: scrap (CPU, default), nvfbc (NVIDIA GPU).
    #[arg(long, default_value = "scrap")]
    capture: String,

    /// Transport protocol: tcp (default) or quic (UDP, better for WAN).
    #[arg(long, default_value = "tcp")]
    transport: String,

    /// Install as auto-start (Windows: logon task, Linux: systemd service).
    #[arg(long)]
    install: bool,

    /// Remove auto-start registration.
    #[arg(long)]
    uninstall: bool,
}

/// Messages from the network receive thread to the main loop.
enum InboundEvent {
    Input(InputEvent),
    Clipboard(String),
    PasteText(String),
    Disconnected,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("phantom=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

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

    #[cfg(target_os = "linux")]
    let use_gpu_pipeline = args.capture == "nvfbc" && args.encoder == "nvenc";
    #[cfg(not(target_os = "linux"))]
    let use_gpu_pipeline = false;

    // GPU zero-copy pipeline (Linux only) or CPU pipeline
    #[cfg(target_os = "linux")]
    let mut gpu = if use_gpu_pipeline {
        Some(GpuPipeline::new(args.fps, args.bitrate)?)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let _gpu: Option<()> = None;

    let mut capture: Option<Box<dyn phantom_core::capture::FrameCapture>> = if !use_gpu_pipeline {
        Some(create_capture(&args.capture)?)
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let (width, height) = if let Some(ref gpu) = gpu {
        (gpu.width, gpu.height)
    } else {
        capture.as_ref().unwrap().resolution()
    };
    #[cfg(not(target_os = "linux"))]
    let (width, height) = capture.as_ref().unwrap().resolution();
    let mut video_encoder: Option<Box<dyn FrameEncoder>> = if !use_gpu_pipeline {
        Some(create_encoder(&args.encoder, width, height, args.fps as f32, args.bitrate)?)
    } else {
        None
    };
    let mut differ = TileDiffer::new();

    // Build transport based on --transport flag
    let tcp_listener;
    let quic_listener;
    let ws_listener: Option<transport_ws::WebServerTransport>;
    match args.transport.as_str() {
        "tcp" => {
            tcp_listener = Some(transport_tcp::TcpServerTransport::bind(&args.listen)?);
            quic_listener = None;
            ws_listener = None;
        }
        "quic" => {
            tcp_listener = None;
            quic_listener = Some(transport_quic::QuicServerTransport::bind(&args.listen)?);
            ws_listener = None;
        }
        "web" => {
            tcp_listener = None;
            quic_listener = None;
            let port: u16 = args.listen.rsplit(':').next()
                .and_then(|p| p.parse().ok()).unwrap_or(9900);
            ws_listener = Some(transport_ws::WebServerTransport::start(
                port, port + 1, port + 2,
            )?);
            tracing::info!("open http://localhost:{port} in browser");
        }
        other => anyhow::bail!("unknown transport '{other}'. Available: tcp, quic, web"),
    }

    loop {
        tracing::info!("waiting for client...");

        let (sender, receiver): (Box<dyn MessageSender>, Box<dyn MessageReceiver>) =
            if let Some(ref ws) = ws_listener {
                // WebRTC only — no WS fallback. Browser POSTs offer to /rtc.
                ws.accept_webrtc()?
            } else if let Some(ref quic) = quic_listener {
                let (s, r) = quic.accept()?;
                (Box::new(s), Box::new(r))
            } else {
                let conn = tcp_listener.as_ref().unwrap().accept_tcp()?;
                if let Some(ref key) = encryption_key {
                    let (s, r) = conn.split_encrypted(key)?;
                    (Box::new(s), Box::new(r))
                } else {
                    let (s, r) = conn.split()?;
                    (Box::new(s), Box::new(r))
                }
            };

        #[cfg(target_os = "linux")]
        let result = if let Some(ref mut gpu) = gpu {
            run_session_gpu(
                &mut gpu.capture, &mut gpu.encoder,
                sender, receiver, frame_interval,
            )
        } else {
            run_session(
                &mut **capture.as_mut().unwrap(),
                &mut **video_encoder.as_mut().unwrap(),
                &mut differ,
                sender, receiver, frame_interval, quality_delay,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let result = run_session(
            &mut **capture.as_mut().unwrap(),
            &mut **video_encoder.as_mut().unwrap(),
            &mut differ,
            sender, receiver, frame_interval, quality_delay,
        );
        if let Err(e) = result {
            tracing::warn!("session ended: {e}");
            if let Some(ref mut enc) = video_encoder {
                differ.reset();
                enc.force_keyframe();
            }
            #[cfg(target_os = "linux")]
            if let Some(ref mut gpu) = gpu {
                let _ = gpu.capture.release_context();
                if let Err(e) = gpu.reset_for_new_session() {
                    tracing::error!("GPU pipeline reset failed: {e}");
                }
            }
        }
    }
}

struct QualityState {
    last_motion: Instant,
    lossless_sent: bool,
    delay: Duration,
}

impl QualityState {
    fn new(delay: Duration) -> Self {
        Self { last_motion: Instant::now(), lossless_sent: false, delay }
    }
    fn on_motion(&mut self) {
        self.last_motion = Instant::now();
        self.lossless_sent = false;
    }
    fn should_send_lossless(&self) -> bool {
        !self.lossless_sent && self.last_motion.elapsed() >= self.delay
    }
    fn mark_lossless_sent(&mut self) { self.lossless_sent = true; }
}

/// Simple congestion tracker: measures send throughput and skips frames if falling behind.
struct CongestionTracker {
    /// Target: one frame interval worth of sending.
    frame_interval: Duration,
    /// Consecutive slow frames.
    slow_frames: u32,
    /// Current skip-every-N (1 = no skip, 2 = skip every other, etc.)
    skip_ratio: u32,
    frame_counter: u64,
}

impl CongestionTracker {
    fn new(frame_interval: Duration) -> Self {
        Self {
            frame_interval,
            slow_frames: 0,
            skip_ratio: 1,
            frame_counter: 0,
        }
    }

    fn should_skip_frame(&mut self) -> bool {
        self.frame_counter += 1;
        if self.skip_ratio <= 1 { return false; }
        !self.frame_counter.is_multiple_of(self.skip_ratio as u64)
    }

    fn on_frame_sent(&mut self, send_duration: Duration) {
        if send_duration > self.frame_interval * 2 {
            self.slow_frames += 1;
            if self.slow_frames > 3 && self.skip_ratio < 4 {
                self.skip_ratio += 1;
                tracing::info!(skip_ratio = self.skip_ratio, "reducing frame rate (congestion)");
            }
        } else {
            if self.slow_frames > 0 { self.slow_frames -= 1; }
            if self.slow_frames == 0 && self.skip_ratio > 1 {
                self.skip_ratio -= 1;
                tracing::info!(skip_ratio = self.skip_ratio, "increasing frame rate (recovered)");
            }
        }
    }
}

#[cfg(target_os = "linux")]
/// GPU zero-copy pipeline: NVFBC capture + NVENC encode, no CPU involvement.
struct GpuPipeline {
    capture: phantom_gpu::nvfbc::NvfbcCapture,
    encoder: phantom_gpu::nvenc::NvencEncoder,
    cuda: std::sync::Arc<phantom_gpu::cuda::CudaLib>,
    ctx: phantom_gpu::sys::CUcontext,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

#[cfg(target_os = "linux")]
impl GpuPipeline {
    fn new(fps: u32, bitrate_kbps: u32) -> Result<Self> {
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

        // Grab one frame to get actual dimensions (may differ from screen size)
        let first = loop {
            std::thread::sleep(Duration::from_millis(20));
            match capture.grab_cuda() {
                Ok(Some(f)) => break f,
                Ok(None) => continue,
                Err(e) => anyhow::bail!("NVFBC initial grab failed: {e}"),
            }
        };
        let (width, height) = (first.width, first.height);
        tracing::info!(screen_w = sw, screen_h = sh, width, height, "NVFBC→NVENC GPU pipeline");

        // Release NVFBC context, init NVENC with shared primary context
        capture.release_context()?;
        let encoder = unsafe {
            phantom_gpu::nvenc::NvencEncoder::with_context(
                std::sync::Arc::clone(&cuda), primary_ctx, false, width, height, fps, bitrate_kbps,
            )?
        };

        Ok(Self { capture, encoder, cuda, ctx: primary_ctx, width, height, fps, bitrate: bitrate_kbps })
    }

    /// Recreate NVENC encoder for a fresh session (clears stale reference frames).
    fn reset_for_new_session(&mut self) -> Result<()> {
        self.capture.reset_session()?;
        // Drop old encoder and create fresh one
        self.encoder = unsafe {
            phantom_gpu::nvenc::NvencEncoder::with_context(
                std::sync::Arc::clone(&self.cuda),
                self.ctx,
                false,
                self.width,
                self.height,
                self.fps,
                self.bitrate,
            )?
        };
        tracing::info!("GPU pipeline reset for new session");
        Ok(())
    }
}

/// Create the screen capture backend based on --capture flag (CPU path only).
fn create_capture(name: &str) -> Result<Box<dyn phantom_core::capture::FrameCapture>> {
    match name {
        "scrap" => {
            let cap = capture_scrap::ScrapCapture::new()?;
            Ok(Box::new(cap))
        }
        other => anyhow::bail!(
            "unknown capture '{other}'. Available: scrap, nvfbc (use with --encoder nvenc for GPU pipeline)"
        ),
    }
}

/// Create the video encoder based on --encoder flag (CPU path only).
fn create_encoder(
    name: &str, width: u32, height: u32, fps: f32, bitrate_kbps: u32,
) -> Result<Box<dyn FrameEncoder>> {
    match name {
        "openh264" => {
            let enc = encode_h264::OpenH264Encoder::new(width, height, fps, bitrate_kbps)?;
            Ok(Box::new(enc))
        }
        "nvenc" => {
            let cuda = std::sync::Arc::new(phantom_gpu::cuda::CudaLib::load()?);
            let enc = phantom_gpu::nvenc::NvencEncoder::new(
                cuda, 0, width, height, fps as u32, bitrate_kbps,
            )?;
            Ok(Box::new(enc))
        }
        other => anyhow::bail!(
            "unknown encoder '{other}'. Available: openh264, nvenc"
        ),
    }
}

#[cfg(target_os = "linux")]
/// GPU zero-copy session loop: NVFBC grab → NVENC encode → send.
/// No tile differ or smart encoding — NVENC at ~4ms is fast enough for every frame.
fn run_session_gpu(
    capture: &mut phantom_gpu::nvfbc::NvfbcCapture,
    encoder: &mut phantom_gpu::nvenc::NvencEncoder,
    mut sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
) -> Result<()> {
    use phantom_core::encode::FrameEncoder;

    encoder.force_keyframe();
    let (width, height) = encoder.dimensions();
    sender.send_msg(&Message::Hello { width, height, format: PixelFormat::Bgra8 })?;
    tracing::info!(width, height, "GPU session started");

    let (event_tx, event_rx) = mpsc::channel::<InboundEvent>();
    std::thread::spawn(move || { receive_loop(receiver, event_tx); });

    let mut injector = input_injector::InputInjector::new().ok();

    hide_remote_cursor();

    let mut clipboard = ClipboardTracker::new();
    let mut arboard = arboard::Clipboard::new().ok();
    let mut clipboard_poll = Instant::now();
    let mut sequence: u64 = 0;
    let mut stats_time = Instant::now();
    let mut stats_frames: u64 = 0;
    let mut stats_bytes: u64 = 0;
    let mut keepalive_time = Instant::now();
    let mut had_input = false;
    // Track consecutive no-frame grabs to back off polling
    let mut no_frame_count: u32 = 0;

    loop {
        let loop_start = Instant::now();

        // Process inbound events
        loop {
            match event_rx.try_recv() {
                Ok(InboundEvent::Input(event)) => {
                    if let Some(ref mut inj) = injector { let _ = inj.inject(&event); }
                    had_input = true;
                }
                Ok(InboundEvent::Clipboard(text)) => {
                    if clipboard.on_remote_update(&text) {
                        if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                    }
                }
                Ok(InboundEvent::PasteText(text)) => {
                    if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                    clipboard.on_remote_update(&text);
                    if let Some(ref mut inj) = injector {
                        let _ = inj.type_text(&text);
                        had_input = true;
                    }
                }
                Ok(InboundEvent::Disconnected) => anyhow::bail!("client disconnected"),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("client disconnected"),
            }
        }

        // Clipboard polling
        if clipboard_poll.elapsed() >= Duration::from_millis(250) {
            clipboard_poll = Instant::now();
            if let Some(ref mut ab) = arboard {
                if let Ok(text) = ab.get_text() {
                    if let Some(changed) = clipboard.check_local_change(&text) {
                        sender.send_msg(&Message::ClipboardSync(changed))?;
                    }
                }
            }
        }

        // GPU capture → encode → send (zero-copy)
        // After input, give the screen a moment to update then grab.
        if had_input {
            std::thread::sleep(Duration::from_millis(2));
            had_input = false;
            no_frame_count = 0; // reset backoff
        }

        capture.bind_context()?;
        let gpu_frame = capture.grab_cuda();
        let _ = capture.release_context();

        match gpu_frame {
            Ok(Some(f)) => {
                no_frame_count = 0;
                let pitch = f.infer_nv12_pitch().unwrap_or(f.width);
                let encoded = encoder.encode_device_nv12(f.device_ptr, pitch)?;
                stats_bytes += encoded.data.len() as u64;
                stats_frames += 1;
                sequence += 1;
                sender.send_msg(&Message::VideoFrame {
                    sequence,
                    frame: Box::new(encoded),
                })?;
            }
            Ok(None) => {
                // No new frame — back off slightly to avoid busy-spinning
                no_frame_count += 1;
                if no_frame_count > 5 {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(e) => {
                tracing::warn!("GPU grab error: {e}");
            }
        }

        // Stats
        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                fps = format_args!("{:.1}", stats_frames as f64 / elapsed),
                bw = format_args!("{:.1} KB/s", stats_bytes as f64 / elapsed / 1024.0),
                "GPU stats"
            );
            stats_time = Instant::now();
            stats_frames = 0;
            stats_bytes = 0;
        }

        // Keepalive
        if keepalive_time.elapsed() >= Duration::from_secs(1) {
            keepalive_time = Instant::now();
            if sender.send_msg(&Message::Ping).is_err() {
                anyhow::bail!("connection lost (keepalive failed)");
            }
        }

        // Frame pacing
        while loop_start.elapsed() < frame_interval {
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    InboundEvent::Input(input) => {
                        if let Some(ref mut inj) = injector { let _ = inj.inject(&input); }
                        had_input = true;
                    }
                    InboundEvent::Clipboard(text) => {
                        if clipboard.on_remote_update(&text) {
                            if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                        }
                    }
                    InboundEvent::PasteText(text) => {
                        if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                        clipboard.on_remote_update(&text);
                        if let Some(ref mut inj) = injector {
                            let _ = inj.type_text(&text);
                            had_input = true;
                        }
                    }
                    InboundEvent::Disconnected => anyhow::bail!("client disconnected"),
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn run_session(
    capture: &mut dyn FrameCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    mut sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
    quality_delay: Duration,
) -> Result<()> {
    // New client needs a keyframe to start decoding
    video_encoder.force_keyframe();
    differ.reset();

    let (width, height) = capture.resolution();
    sender.send_msg(&Message::Hello { width, height, format: PixelFormat::Bgra8 })?;
    tracing::info!(width, height, "session started");

    let (event_tx, event_rx) = mpsc::channel::<InboundEvent>();
    std::thread::spawn(move || { receive_loop(receiver, event_tx); });

    let mut injector = input_injector::InputInjector::new().ok();

    // Hide remote cursor — client renders its own local cursor.
    // This prevents mouse movement from causing dirty tiles on the server.
    hide_remote_cursor();

    let mut zstd_encoder = encode_zstd::ZstdEncoder::new(3);
    let mut quality = QualityState::new(quality_delay);
    let mut congestion = CongestionTracker::new(frame_interval);
    let mut clipboard = ClipboardTracker::new();
    let mut arboard = arboard::Clipboard::new().ok();
    let mut clipboard_poll = Instant::now();

    let mut sequence: u64 = 0;
    let mut stats_time = Instant::now();
    let mut keepalive_time = Instant::now();
    let mut stats_h264: u64 = 0;
    let mut stats_tiles: u64 = 0;
    let mut stats_lossless: u64 = 0;
    let mut stats_bytes: u64 = 0;
    let mut last_frame: Option<Frame> = None;
    let mut had_input = false;

    loop {
        let loop_start = Instant::now();

        // Process inbound events (input + clipboard from client)
        loop {
            match event_rx.try_recv() {
                Ok(InboundEvent::Input(event)) => {
                    if let Some(ref mut inj) = injector { let _ = inj.inject(&event); }
                    had_input = true;
                }
                Ok(InboundEvent::Clipboard(text)) => {
                    if clipboard.on_remote_update(&text) {
                        if let Some(ref mut ab) = arboard {
                            let _ = ab.set_text(&text);
                        }
                    }
                }
                Ok(InboundEvent::PasteText(text)) => {
                    // Set clipboard AND type it out
                    if let Some(ref mut ab) = arboard {
                        let _ = ab.set_text(&text);
                    }
                    clipboard.on_remote_update(&text);
                    if let Some(ref mut inj) = injector {
                        let _ = inj.type_text(&text);
                        had_input = true;
                    }
                    tracing::debug!("paste: {} chars", text.len());
                }
                Ok(InboundEvent::Disconnected) => {
                    anyhow::bail!("client disconnected");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("client disconnected");
                }
            }
        }

        // Poll local clipboard every 250ms
        if clipboard_poll.elapsed() >= Duration::from_millis(250) {
            clipboard_poll = Instant::now();
            if let Some(ref mut ab) = arboard {
                if let Ok(text) = ab.get_text() {
                    if let Some(changed) = clipboard.check_local_change(&text) {
                        sender.send_msg(&Message::ClipboardSync(changed))?;
                    }
                }
            }
        }

        // Capture
        let frame = match capture.capture()? {
            Some(f) => f,
            None => {
                if quality.should_send_lossless() {
                    if let Some(ref f) = last_frame {
                        send_lossless_update(&mut *sender, &mut zstd_encoder, f, &mut sequence)?;
                        quality.mark_lossless_sent();
                        stats_lossless += 1;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        };

        // After input injection, always encode (screen likely changed in ways
        // that sampling-based has_changes() might miss, e.g. xeyes, cursor blink).
        let changed = had_input || differ.has_changes(&frame);
        if changed {
            had_input = false;
            let dirty_tiles = differ.diff(&frame);
            quality.on_motion();

            if congestion.should_skip_frame() {
                last_frame = Some(frame);
                continue;
            }

            // Always encode full H.264 frame when there are changes.
            // Tile mode caused visual tearing when mixed with H.264 over high latency.
            if !dirty_tiles.is_empty() {
                let encoded = video_encoder.encode_frame(&frame)?;
                stats_bytes += encoded.data.len() as u64;
                stats_h264 += 1;
                sequence += 1;
                let send_start = Instant::now();
                sender.send_msg(&Message::VideoFrame { sequence, frame: Box::new(encoded) })?;
                congestion.on_frame_sent(send_start.elapsed());
            }

            last_frame = Some(frame);
        }

        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                h264 = format_args!("{:.1}/s", stats_h264 as f64 / elapsed),
                tiles = format_args!("{:.1}/s", stats_tiles as f64 / elapsed),
                lossless = stats_lossless,
                bw = format_args!("{:.1} KB/s", stats_bytes as f64 / elapsed / 1024.0),
                "stats"
            );
            stats_time = Instant::now();
            stats_h264 = 0;
            stats_tiles = 0;
            stats_lossless = 0;
            stats_bytes = 0;

        }

        // Keepalive every 1s: detect dead connection (channel dropped on browser refresh)
        if keepalive_time.elapsed() >= Duration::from_secs(1) {
            keepalive_time = Instant::now();
            if sender.send_msg(&Message::Ping).is_err() {
                anyhow::bail!("connection lost (keepalive failed)");
            }
        }

        // Frame pacing: sleep in small increments, processing input between each.
        while loop_start.elapsed() < frame_interval {
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    InboundEvent::Input(input) => {
                        if let Some(ref mut inj) = injector { let _ = inj.inject(&input); }
                        had_input = true; // will force encode next iteration
                    }
                    InboundEvent::Clipboard(text) => {
                        if clipboard.on_remote_update(&text) {
                            if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                        }
                    }
                    InboundEvent::PasteText(text) => {
                        if let Some(ref mut ab) = arboard { let _ = ab.set_text(&text); }
                        clipboard.on_remote_update(&text);
                        if let Some(ref mut inj) = injector {
                            let _ = inj.type_text(&text);
                            had_input = true;
                        }
                    }
                    InboundEvent::Disconnected => {
                        anyhow::bail!("client disconnected");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn send_lossless_update(
    sender: &mut dyn MessageSender, encoder: &mut encode_zstd::ZstdEncoder,
    frame: &Frame, sequence: &mut u64,
) -> Result<usize> {
    // Use a fresh differ to get ALL tiles (first frame = all dirty).
    // Don't touch the main differ's state.
    let mut fresh = TileDiffer::new();
    let all_tiles = fresh.diff(frame);
    let encoded = encoder.encode_tiles(&all_tiles)?;
    let total_bytes: usize = encoded.iter().map(|t| t.data.len()).sum();
    *sequence += 1;
    sender.send_msg(&Message::TileUpdate { sequence: *sequence, tiles: Box::new(encoded) })?;
    Ok(total_bytes)
}

fn receive_loop(mut receiver: Box<dyn MessageReceiver>, tx: mpsc::Sender<InboundEvent>) {
    loop {
        match receiver.recv_msg() {
            Ok(Message::Input(event)) => { let _ = tx.send(InboundEvent::Input(event)); }
            Ok(Message::ClipboardSync(text)) => { let _ = tx.send(InboundEvent::Clipboard(text)); }
            Ok(Message::PasteText(text)) => { let _ = tx.send(InboundEvent::PasteText(text)); }
            Ok(_) => {}
            Err(_) => { let _ = tx.send(InboundEvent::Disconnected); break; }
        }
    }
}

/// Hide the remote OS cursor so mouse movement doesn't cause dirty tiles.
/// Client renders its own local cursor instead.
fn hide_remote_cursor() {
    #[cfg(target_os = "linux")]
    {
        // Create a 1x1 transparent cursor using xdotool/unclutter or xfixes
        // Try multiple methods in order of preference
        if std::process::Command::new("unclutter")
            .args(["-idle", "0", "-root"])
            .spawn()
            .is_ok()
        {
            tracing::info!("remote cursor hidden (unclutter)");
            return;
        }

        // Fallback: use xdotool to set blank cursor on root window
        if std::process::Command::new("xdotool")
            .args(["search", "--name", ".*"])
            .output()
            .is_ok()
        {
            // xdotool can't directly hide cursor, but we tried
            tracing::debug!("xdotool available but cursor hiding limited");
        }

        tracing::debug!("could not hide remote cursor (install 'unclutter' for best results)");
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::debug!("remote cursor hiding not implemented for this OS");
    }
}

/// Install phantom-server to auto-start on login.
fn install_autostart() -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("get current exe path")?;
    #[allow(unused_variables)]
    let exe_str = exe.to_string_lossy();

    #[cfg(target_os = "windows")]
    {
        // Windows: create a scheduled task that runs at logon in the interactive session
        let status = std::process::Command::new("schtasks")
            .args([
                "/Create", "/TN", "PhantomServer",
                "/TR", &format!("\"{exe_str}\" --no-encrypt --transport web"),
                "/SC", "ONLOGON",
                "/RL", "HIGHEST",
                "/IT",  // interactive — runs in the user's desktop session
                "/F",   // force overwrite
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
        // Linux: install systemd user service
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

/// Remove auto-start registration.
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
