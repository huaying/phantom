mod decode_zstd;
mod display_minifb;
mod input_capture;
mod transport_tcp;

use anyhow::{bail, Result};
use clap::Parser;
use phantom_core::decode::Decoder;
use phantom_core::display::Display;
use phantom_core::protocol::Message;
use phantom_core::transport::{Connection, MessageReceiver, MessageSender};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "phantom-client", about = "Phantom remote desktop client")]
struct Args {
    /// Server address to connect to.
    #[arg(short, long, default_value = "127.0.0.1:9900")]
    connect: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("phantom=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    // Connect and receive Hello
    let transport = transport_tcp::TcpClientTransport::new(&args.connect);
    let mut conn = transport.connect_tcp()?;

    let (width, height) = match conn.recv_msg()? {
        Message::Hello { width, height, .. } => {
            tracing::info!(width, height, "received hello");
            (width, height)
        }
        _ => bail!("expected Hello message"),
    };

    // Split connection for bidirectional use
    let (sender, receiver) = conn.split()?;

    // Init display (must be on main thread for macOS)
    let mut display = display_minifb::MinifbDisplay::new();
    display.init(width, height)?;

    let mut decoder = decode_zstd::ZstdDecoder::new();
    let mut input_capture = input_capture::InputCapture::new();

    // Spawn network receive thread
    let (frame_tx, frame_rx) = mpsc::channel::<Message>();
    std::thread::spawn(move || {
        recv_loop(receiver, frame_tx);
    });

    // Input sender runs in a separate thread too (to not block main loop on TCP write)
    let (input_tx, input_rx) = mpsc::channel::<Message>();
    std::thread::spawn(move || {
        input_send_loop(sender, input_rx);
    });

    // -- Main loop: display + input capture --
    let mut stats_time = Instant::now();
    let mut stats_frames: u64 = 0;

    loop {
        // Drain frame updates (use latest only)
        let mut last_update = None;
        while let Ok(msg) = frame_rx.try_recv() {
            if let Message::FrameUpdate { .. } = &msg {
                last_update = Some(msg);
            }
        }

        // Decode and composite
        if let Some(Message::FrameUpdate { tiles, .. }) = last_update {
            let mut decoded = Vec::with_capacity(tiles.len());
            for tile in &tiles {
                decoded.push(decoder.decode_tile(tile)?);
            }
            display.update_tiles(&decoded)?;
            stats_frames += 1;
        }

        // Present
        if !display.present()? {
            tracing::info!("window closed");
            break;
        }

        // Capture and send input events
        let events = input_capture.poll(display.window().unwrap());
        for event in events {
            // Best-effort send; if channel is full, drop input
            let _ = input_tx.send(Message::Input(event));
        }

        // Stats
        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                fps = format_args!("{:.1}", stats_frames as f64 / elapsed),
                "stats"
            );
            stats_time = Instant::now();
            stats_frames = 0;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

fn recv_loop(mut receiver: transport_tcp::TcpReceiver, tx: mpsc::Sender<Message>) {
    loop {
        match receiver.recv_msg() {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!("receive ended: {e}");
                break;
            }
        }
    }
}

fn input_send_loop(mut sender: transport_tcp::TcpSender, rx: mpsc::Receiver<Message>) {
    while let Ok(msg) = rx.recv() {
        if let Err(e) = sender.send_msg(&msg) {
            tracing::debug!("input send ended: {e}");
            break;
        }
    }
}
