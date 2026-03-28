mod capture_scrap;
mod encode_h264;
mod encode_zstd;
mod input_injector;
mod transport_tcp;

use anyhow::Result;
use clap::Parser;
use phantom_core::capture::FrameCapture;
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
    /// Address to listen on.
    #[arg(short, long, default_value = "0.0.0.0:9900")]
    listen: String,

    /// Target frames per second.
    #[arg(short, long, default_value_t = 30)]
    fps: u32,

    /// Target bitrate in kbps for H.264.
    #[arg(short, long, default_value_t = 5000)]
    bitrate: u32,

    /// Milliseconds of stillness before sending lossless quality update.
    #[arg(long, default_value_t = 500)]
    quality_delay_ms: u64,

    /// Encryption key (64 hex chars). If omitted, generates a new key.
    /// Pass --no-encrypt to disable encryption entirely.
    #[arg(short, long)]
    key: Option<String>,

    /// Disable encryption (insecure, for testing only).
    #[arg(long)]
    no_encrypt: bool,
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

    // Resolve encryption key
    let encryption_key: Option<[u8; 32]> = if args.no_encrypt {
        tracing::warn!("encryption DISABLED — traffic is plaintext");
        None
    } else {
        let key = match &args.key {
            Some(hex) => crypto::parse_key_hex(hex)?,
            None => {
                let hex = crypto::generate_key_hex();
                tracing::info!("generated encryption key (pass to client with --key):");
                eprintln!("\n  --key {hex}\n");
                crypto::parse_key_hex(&hex)?
            }
        };
        tracing::info!("encryption ENABLED (ChaCha20-Poly1305)");
        Some(key)
    };

    let mut capture = capture_scrap::ScrapCapture::new()?;
    let (width, height) = capture.resolution();

    let mut h264_encoder =
        encode_h264::OpenH264Encoder::new(width, height, args.fps as f32, args.bitrate)?;
    let mut differ = TileDiffer::new();
    let listener = transport_tcp::TcpServerTransport::bind(&args.listen)?;

    loop {
        tracing::info!("waiting for client...");
        let conn = listener.accept_tcp()?;

        // Split with or without encryption
        let (sender, receiver): (Box<dyn MessageSender>, Box<dyn MessageReceiver>) =
            if let Some(ref key) = encryption_key {
                let (s, r) = conn.split_encrypted(key)?;
                (Box::new(s), Box::new(r))
            } else {
                let (s, r) = conn.split()?;
                (Box::new(s), Box::new(r))
            };

        if let Err(e) = run_session(
            &mut capture,
            &mut h264_encoder,
            &mut differ,
            sender,
            receiver,
            frame_interval,
            quality_delay,
        ) {
            tracing::warn!("session ended: {e}");
            differ.reset();
            h264_encoder.force_keyframe();
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
    fn mark_lossless_sent(&mut self) {
        self.lossless_sent = true;
    }
}

fn run_session(
    capture: &mut capture_scrap::ScrapCapture,
    h264_encoder: &mut encode_h264::OpenH264Encoder,
    differ: &mut TileDiffer,
    mut sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
    quality_delay: Duration,
) -> Result<()> {
    let (width, height) = capture.resolution();
    sender.send_msg(&Message::Hello {
        width,
        height,
        format: PixelFormat::Bgra8,
    })?;
    tracing::info!(width, height, "session started");

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    std::thread::spawn(move || {
        input_receive_loop(receiver, input_tx);
    });

    let mut injector = match input_injector::InputInjector::new() {
        Ok(inj) => Some(inj),
        Err(e) => {
            tracing::warn!("input injection unavailable: {e}");
            None
        }
    };

    let mut zstd_encoder = encode_zstd::ZstdEncoder::new(3);
    let mut quality = QualityState::new(quality_delay);
    let mut sequence: u64 = 0;
    let mut stats_time = Instant::now();
    let mut stats_h264: u64 = 0;
    let mut stats_lossless: u64 = 0;
    let mut stats_bytes: u64 = 0;
    let mut last_frame: Option<Frame> = None;

    loop {
        let loop_start = Instant::now();

        while let Ok(event) = input_rx.try_recv() {
            if let Some(ref mut inj) = injector {
                let _ = inj.inject(&event);
            }
        }

        let frame = match capture.capture()? {
            Some(f) => f,
            None => {
                if quality.should_send_lossless() {
                    if let Some(ref f) = last_frame {
                        send_lossless_update(
                            &mut *sender, &mut zstd_encoder, differ, f, &mut sequence,
                        )?;
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

            let encoded = h264_encoder.encode_frame(&frame)?;
            stats_bytes += encoded.data.len() as u64;
            stats_h264 += 1;
            sequence += 1;
            sender.send_msg(&Message::VideoFrame { sequence, frame: encoded })?;
            last_frame = Some(frame);
        } else if quality.should_send_lossless() {
            if let Some(ref f) = last_frame {
                let bytes = send_lossless_update(
                    &mut *sender, &mut zstd_encoder, differ, f, &mut sequence,
                )?;
                stats_bytes += bytes as u64;
                quality.mark_lossless_sent();
                stats_lossless += 1;
                tracing::info!(bytes = format_args!("{:.1} KB", bytes as f64 / 1024.0), "quality update");
            }
        }

        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                h264_fps = format_args!("{:.1}", stats_h264 as f64 / elapsed),
                lossless = stats_lossless,
                bw = format_args!("{:.1} KB/s", stats_bytes as f64 / elapsed / 1024.0),
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
    sender: &mut dyn MessageSender,
    encoder: &mut encode_zstd::ZstdEncoder,
    differ: &mut TileDiffer,
    frame: &Frame,
    sequence: &mut u64,
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

fn input_receive_loop(
    mut receiver: Box<dyn MessageReceiver>,
    tx: mpsc::Sender<InputEvent>,
) {
    loop {
        match receiver.recv_msg() {
            Ok(Message::Input(event)) => {
                if tx.send(event).is_err() { break; }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("input receive ended: {e}");
                break;
            }
        }
    }
}
