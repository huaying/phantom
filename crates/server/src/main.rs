mod capture_scrap;
mod encode_zstd;
mod input_injector;
mod transport_tcp;

use anyhow::Result;
use clap::Parser;
use phantom_core::capture::FrameCapture;
use phantom_core::encode::Encoder;
use phantom_core::frame::PixelFormat;
use phantom_core::input::InputEvent;
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::MessageSender;
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

    /// Zstd compression level (1-22).
    #[arg(short, long, default_value_t = 3)]
    compression: i32,
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

    let mut capture = capture_scrap::ScrapCapture::new()?;
    let mut encoder = encode_zstd::ZstdEncoder::new(args.compression);
    let mut differ = TileDiffer::new();
    let listener = transport_tcp::TcpServerTransport::bind(&args.listen)?;

    loop {
        tracing::info!("waiting for client...");
        let conn = listener.accept_tcp()?;

        if let Err(e) = run_session(&mut capture, &mut encoder, &mut differ, conn, frame_interval)
        {
            tracing::warn!("session ended: {e}");
            differ = TileDiffer::new();
        }
    }
}

fn run_session(
    capture: &mut capture_scrap::ScrapCapture,
    encoder: &mut encode_zstd::ZstdEncoder,
    differ: &mut TileDiffer,
    conn: transport_tcp::TcpConnection,
    frame_interval: Duration,
) -> Result<()> {
    let (mut sender, receiver) = conn.split()?;

    // Send hello
    let (width, height) = capture.resolution();
    sender.send_msg(&Message::Hello {
        width,
        height,
        format: PixelFormat::Bgra8,
    })?;
    tracing::info!(width, height, "session started");

    // Spawn input receive thread
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    std::thread::spawn(move || {
        input_receive_loop(receiver, input_tx);
    });

    // Init input injector
    let mut injector = match input_injector::InputInjector::new() {
        Ok(inj) => Some(inj),
        Err(e) => {
            tracing::warn!("input injection unavailable: {e}");
            tracing::warn!("grant Accessibility permission to enable remote control");
            None
        }
    };

    let mut sequence: u64 = 0;
    let mut stats_time = Instant::now();
    let mut stats_frames: u64 = 0;
    let mut stats_bytes: u64 = 0;

    loop {
        let loop_start = Instant::now();

        // Process pending input events (non-blocking)
        while let Ok(event) = input_rx.try_recv() {
            if let Some(ref mut inj) = injector {
                if let Err(e) = inj.inject(&event) {
                    tracing::debug!("input inject error: {e}");
                }
            }
        }

        // Capture
        let frame = match capture.capture()? {
            Some(f) => f,
            None => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        };

        // Diff
        let dirty = differ.diff(&frame);
        if dirty.is_empty() {
            let elapsed = loop_start.elapsed();
            if elapsed < frame_interval {
                std::thread::sleep(frame_interval - elapsed);
            }
            continue;
        }

        // Encode & send
        let encoded = encoder.encode_tiles(&dirty)?;
        let frame_bytes: usize = encoded.iter().map(|t| t.data.len()).sum();
        stats_bytes += frame_bytes as u64;
        stats_frames += 1;

        sequence += 1;
        sender.send_msg(&Message::FrameUpdate {
            sequence,
            tiles: encoded,
        })?;

        // Stats every 5s
        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                fps = format_args!("{:.1}", stats_frames as f64 / elapsed),
                bandwidth = format_args!("{:.1} KB/s", stats_bytes as f64 / elapsed / 1024.0),
                "stats"
            );
            stats_time = Instant::now();
            stats_frames = 0;
            stats_bytes = 0;
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
}

fn input_receive_loop(
    mut receiver: transport_tcp::TcpReceiver,
    tx: mpsc::Sender<InputEvent>,
) {
    use phantom_core::transport::MessageReceiver;
    loop {
        match receiver.recv_msg() {
            Ok(Message::Input(event)) => {
                if tx.send(event).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("input receive ended: {e}");
                break;
            }
        }
    }
}
