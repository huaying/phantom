mod capture_scrap;
mod encode_h264;
mod encode_zstd;
mod input_injector;
mod transport_tcp;

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
    #[arg(long, default_value_t = 500)]
    quality_delay_ms: u64,
    #[arg(short, long)]
    key: Option<String>,
    #[arg(long)]
    no_encrypt: bool,

    /// Video encoder: openh264 (default). Future: x264, nvenc, vaapi.
    #[arg(long, default_value = "openh264")]
    encoder: String,
}

/// Messages from the network receive thread to the main loop.
enum InboundEvent {
    Input(InputEvent),
    Clipboard(String),
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

    let mut capture = capture_scrap::ScrapCapture::new()?;
    let (width, height) = capture.resolution();
    let mut video_encoder: Box<dyn FrameEncoder> = create_encoder(
        &args.encoder, width, height, args.fps as f32, args.bitrate,
    )?;
    let mut differ = TileDiffer::new();
    let listener = transport_tcp::TcpServerTransport::bind(&args.listen)?;

    loop {
        tracing::info!("waiting for client...");
        let conn = listener.accept_tcp()?;
        let (sender, receiver): (Box<dyn MessageSender>, Box<dyn MessageReceiver>) =
            if let Some(ref key) = encryption_key {
                let (s, r) = conn.split_encrypted(key)?;
                (Box::new(s), Box::new(r))
            } else {
                let (s, r) = conn.split()?;
                (Box::new(s), Box::new(r))
            };

        if let Err(e) = run_session(
            &mut capture, &mut *video_encoder, &mut differ,
            sender, receiver, frame_interval, quality_delay,
        ) {
            tracing::warn!("session ended: {e}");
            differ.reset();
            video_encoder.force_keyframe();
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
        self.frame_counter % self.skip_ratio as u64 != 0
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

/// Create the video encoder based on --encoder flag.
/// Currently only openh264. Add new backends here.
fn create_encoder(
    name: &str, width: u32, height: u32, fps: f32, bitrate_kbps: u32,
) -> Result<Box<dyn FrameEncoder>> {
    match name {
        "openh264" => {
            let enc = encode_h264::OpenH264Encoder::new(width, height, fps, bitrate_kbps)?;
            Ok(Box::new(enc))
        }
        // Future backends:
        // "x264" => { ... }
        // "nvenc" => { ... }
        // "vaapi" => { ... }
        other => anyhow::bail!(
            "unknown encoder '{}'. Available: openh264. Future: x264, nvenc, vaapi", other
        ),
    }
}

fn run_session(
    capture: &mut capture_scrap::ScrapCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    mut sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
    quality_delay: Duration,
) -> Result<()> {
    let (width, height) = capture.resolution();
    sender.send_msg(&Message::Hello { width, height, format: PixelFormat::Bgra8 })?;
    tracing::info!(width, height, "session started");

    let (event_tx, event_rx) = mpsc::channel::<InboundEvent>();
    std::thread::spawn(move || { receive_loop(receiver, event_tx); });

    let mut injector = match input_injector::InputInjector::new() {
        Ok(inj) => Some(inj),
        Err(e) => { tracing::warn!("input injection unavailable: {e}"); None }
    };

    let mut zstd_encoder = encode_zstd::ZstdEncoder::new(3);
    let mut quality = QualityState::new(quality_delay);
    let mut congestion = CongestionTracker::new(frame_interval);
    let mut clipboard = ClipboardTracker::new();
    let mut arboard = arboard::Clipboard::new().ok();
    let mut clipboard_poll = Instant::now();

    let mut sequence: u64 = 0;
    let mut stats_time = Instant::now();
    let mut stats_h264: u64 = 0;
    let mut stats_lossless: u64 = 0;
    let mut stats_bytes: u64 = 0;
    let mut last_frame: Option<Frame> = None;

    loop {
        let loop_start = Instant::now();

        // Process inbound events (input + clipboard from client)
        loop {
            match event_rx.try_recv() {
                Ok(InboundEvent::Input(event)) => {
                    if let Some(ref mut inj) = injector { let _ = inj.inject(&event); }
                }
                Ok(InboundEvent::Clipboard(text)) => {
                    if clipboard.on_remote_update(&text) {
                        if let Some(ref mut ab) = arboard {
                            let _ = ab.set_text(&text);
                            tracing::debug!("clipboard received from client ({} chars)", text.len());
                        }
                    }
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
                        send_lossless_update(&mut *sender, &mut zstd_encoder, differ, f, &mut sequence)?;
                        quality.mark_lossless_sent();
                        stats_lossless += 1;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        };

        if differ.has_changes(&frame) {
            differ.diff(&frame);
            quality.on_motion();

            // Congestion: skip frame if we're falling behind
            if congestion.should_skip_frame() {
                last_frame = Some(frame);
                continue;
            }

            let encoded = video_encoder.encode_frame(&frame)?;
            stats_bytes += encoded.data.len() as u64;
            stats_h264 += 1;
            sequence += 1;

            let send_start = Instant::now();
            sender.send_msg(&Message::VideoFrame { sequence, frame: encoded })?;
            congestion.on_frame_sent(send_start.elapsed());

            last_frame = Some(frame);
        } else if quality.should_send_lossless() {
            if let Some(ref f) = last_frame {
                let bytes = send_lossless_update(&mut *sender, &mut zstd_encoder, differ, f, &mut sequence)?;
                stats_bytes += bytes as u64;
                quality.mark_lossless_sent();
                stats_lossless += 1;
            }
        }

        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                h264_fps = format_args!("{:.1}", stats_h264 as f64 / elapsed),
                lossless = stats_lossless,
                bw = format_args!("{:.1} KB/s", stats_bytes as f64 / elapsed / 1024.0),
                skip = format_args!("1/{}", congestion.skip_ratio),
                "stats"
            );
            stats_time = Instant::now();
            stats_h264 = 0;
            stats_lossless = 0;
            stats_bytes = 0;
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}

fn send_lossless_update(
    sender: &mut dyn MessageSender, encoder: &mut encode_zstd::ZstdEncoder,
    differ: &mut TileDiffer, frame: &Frame, sequence: &mut u64,
) -> Result<usize> {
    let saved = std::mem::replace(differ, TileDiffer::new());
    let all_tiles = differ.diff(frame);
    *differ = saved;
    differ.diff(frame);
    let encoded = encoder.encode_tiles(&all_tiles)?;
    let total_bytes: usize = encoded.iter().map(|t| t.data.len()).sum();
    *sequence += 1;
    sender.send_msg(&Message::TileUpdate { sequence: *sequence, tiles: encoded })?;
    Ok(total_bytes)
}

fn receive_loop(mut receiver: Box<dyn MessageReceiver>, tx: mpsc::Sender<InboundEvent>) {
    loop {
        match receiver.recv_msg() {
            Ok(Message::Input(event)) => { let _ = tx.send(InboundEvent::Input(event)); }
            Ok(Message::ClipboardSync(text)) => { let _ = tx.send(InboundEvent::Clipboard(text)); }
            Ok(_) => {}
            Err(_) => { let _ = tx.send(InboundEvent::Disconnected); break; }
        }
    }
}
