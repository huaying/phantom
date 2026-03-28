mod decode_h264;
mod decode_zstd;
mod display_minifb;
mod input_capture;
mod transport_tcp;

use anyhow::{bail, Result};
use clap::Parser;
use phantom_core::crypto;
use phantom_core::decode::Decoder;
use phantom_core::display::Display;
use phantom_core::encode::FrameDecoder;
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "phantom-client", about = "Phantom remote desktop client")]
struct Args {
    /// Server address to connect to.
    #[arg(short, long, default_value = "127.0.0.1:9900")]
    connect: String,

    /// Encryption key (64 hex chars, must match server).
    #[arg(short, long)]
    key: Option<String>,

    /// Disable encryption (must match server's --no-encrypt).
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

    let encryption_key: Option<[u8; 32]> = if args.no_encrypt {
        tracing::warn!("encryption DISABLED");
        None
    } else {
        match &args.key {
            Some(hex) => {
                tracing::info!("encryption ENABLED");
                Some(crypto::parse_key_hex(hex)?)
            }
            None => bail!("--key required (copy from server output), or use --no-encrypt"),
        }
    };

    let transport = transport_tcp::TcpClientTransport::new(&args.connect);
    let conn = transport.connect_tcp()?;

    // Split with or without encryption
    let (sender, mut receiver): (Box<dyn MessageSender>, Box<dyn MessageReceiver>) =
        if let Some(ref key) = encryption_key {
            let (s, r) = conn.split_encrypted(key)?;
            (Box::new(s), Box::new(r))
        } else {
            let (s, r) = conn.split()?;
            (Box::new(s), Box::new(r))
        };

    // Receive Hello
    let (width, height) = match receiver.recv_msg()? {
        Message::Hello { width, height, .. } => {
            tracing::info!(width, height, "connected");
            (width, height)
        }
        _ => bail!("expected Hello message"),
    };

    let mut display = display_minifb::MinifbDisplay::new();
    display.init(width, height)?;

    let mut h264_decoder = decode_h264::OpenH264Decoder::new(width, height)?;
    let mut tile_decoder = decode_zstd::ZstdDecoder::new();
    let mut input_capture = input_capture::InputCapture::new();

    let (frame_tx, frame_rx) = mpsc::channel::<Message>();
    std::thread::spawn(move || recv_loop(receiver, frame_tx));

    let (input_tx, input_rx) = mpsc::channel::<Message>();
    std::thread::spawn(move || input_send_loop(sender, input_rx));

    let mut stats_time = Instant::now();
    let mut stats_video: u64 = 0;
    let mut stats_tiles: u64 = 0;

    loop {
        let mut last_video = None;
        let mut last_tiles = None;
        while let Ok(msg) = frame_rx.try_recv() {
            match msg {
                Message::VideoFrame { .. } => last_video = Some(msg),
                Message::TileUpdate { .. } => last_tiles = Some(msg),
                _ => {}
            }
        }

        if let Some(Message::VideoFrame { frame, .. }) = last_video {
            match h264_decoder.decode_frame(&frame.data) {
                Ok(rgb32) => {
                    display.update_full_frame(&rgb32);
                    stats_video += 1;
                }
                Err(e) => tracing::debug!("H.264 decode error: {e}"),
            }
        }

        if let Some(Message::TileUpdate { tiles, .. }) = last_tiles {
            let mut decoded = Vec::with_capacity(tiles.len());
            for tile in &tiles {
                match tile_decoder.decode_tile(tile) {
                    Ok(dt) => decoded.push(dt),
                    Err(e) => tracing::debug!("tile decode error: {e}"),
                }
            }
            display.update_tiles(&decoded)?;
            stats_tiles += 1;
        }

        if !display.present()? {
            break;
        }

        let events = input_capture.poll(display.window().unwrap());
        for event in events {
            let _ = input_tx.send(Message::Input(event));
        }

        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                video_fps = format_args!("{:.1}", stats_video as f64 / elapsed),
                quality_updates = stats_tiles,
                "stats"
            );
            stats_time = Instant::now();
            stats_video = 0;
            stats_tiles = 0;
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

fn recv_loop(mut receiver: Box<dyn MessageReceiver>, tx: mpsc::Sender<Message>) {
    loop {
        match receiver.recv_msg() {
            Ok(msg) => { if tx.send(msg).is_err() { break; } }
            Err(e) => { tracing::debug!("receive ended: {e}"); break; }
        }
    }
}

fn input_send_loop(mut sender: Box<dyn MessageSender>, rx: mpsc::Receiver<Message>) {
    while let Ok(msg) = rx.recv() {
        if let Err(e) = sender.send_msg(&msg) {
            tracing::debug!("input send ended: {e}");
            break;
        }
    }
}
