mod cursor;
mod decode_h264;
mod decode_zstd;
mod display_minifb;
mod input_capture;
mod transport_quic;
mod transport_tcp;

use anyhow::{bail, Result};
use clap::Parser;
use phantom_core::clipboard::ClipboardTracker;
use phantom_core::crypto;
use phantom_core::decode::Decoder;
use phantom_core::display::Display;
use phantom_core::encode::FrameDecoder;
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
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

    /// Transport protocol: tcp (default) or quic (must match server).
    #[arg(long, default_value = "tcp")]
    transport: String,
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

    // For QUIC, we need the transport to persist across reconnects (holds the runtime)
    let quic_transport = if args.transport == "quic" {
        Some(transport_quic::QuicClientTransport::new()?)
    } else {
        None
    };

    let mut display: Option<display_minifb::MinifbDisplay> = None;
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(10);

    loop {
        // -- Connect --
        tracing::info!(addr = %args.connect, transport = %args.transport, "connecting...");

        let connect_result: Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> =
            if let Some(ref quic) = quic_transport {
                // QUIC: encryption built-in, no need for ChaCha20 layer
                match quic.connect(&args.connect) {
                    Ok((s, r)) => Ok((Box::new(s), Box::new(r))),
                    Err(e) => Err(e),
                }
            } else {
                // TCP
                match transport_tcp::TcpClientTransport::new(&args.connect).connect_tcp() {
                    Ok(conn) => {
                        if let Some(ref key) = encryption_key {
                            let (s, r) = conn.split_encrypted(key)?;
                            Ok((Box::new(s), Box::new(r)))
                        } else {
                            let (s, r) = conn.split()?;
                            Ok((Box::new(s), Box::new(r)))
                        }
                    }
                    Err(e) => Err(e),
                }
            };

        let (sender, mut receiver) = match connect_result {
            Ok(pair) => {
                backoff = Duration::from_millis(500);
                pair
            }
            Err(e) => {
                tracing::warn!("connection failed: {e}, retrying in {:.1}s", backoff.as_secs_f32());
                if let Some(ref mut d) = display {
                    if !d.present().unwrap_or(false) { break; }
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        // Receive Hello
        let (width, height) = match receiver.recv_msg() {
            Ok(Message::Hello { width, height, .. }) => {
                if width == 0 || height == 0 || width > 8192 || height > 8192 {
                    tracing::warn!(width, height, "invalid resolution from server");
                    continue;
                }
                tracing::info!(width, height, "connected");
                (width, height)
            }
            Ok(_) => { tracing::warn!("unexpected message, expected Hello"); continue; }
            Err(e) => { tracing::warn!("handshake failed: {e}"); continue; }
        };

        // Init or reinit display
        if display.is_none() {
            let mut d = display_minifb::MinifbDisplay::new();
            d.init(width, height)?;
            display = Some(d);
        }

        // Run session (returns on disconnect)
        let should_quit = run_session(
            display.as_mut().expect("display must be initialized"),
            sender,
            receiver,
            width,
            height,
        )?;

        if should_quit {
            break;
        }

        tracing::info!("disconnected, reconnecting in {:.1}s...", backoff.as_secs_f32());
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(max_backoff);
    }

    Ok(())
}

/// Run one session. Returns Ok(true) if window was closed (quit), Ok(false) on disconnect.
fn run_session(
    display: &mut display_minifb::MinifbDisplay,
    sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    width: u32,
    height: u32,
) -> Result<bool> {
    let mut h264_decoder = decode_h264::OpenH264Decoder::new(width, height)?;
    let mut tile_decoder = decode_zstd::ZstdDecoder::new();
    let mut input_capture = input_capture::InputCapture::new();
    let mut clipboard = ClipboardTracker::new();
    let mut arboard = arboard::Clipboard::new().ok();
    let mut clipboard_poll = Instant::now();

    let connected = Arc::new(AtomicBool::new(true));

    let (frame_tx, frame_rx) = mpsc::channel::<Message>();
    let recv_connected = connected.clone();
    std::thread::spawn(move || {
        recv_loop(receiver, frame_tx);
        recv_connected.store(false, Ordering::Relaxed);
    });

    let (input_tx, input_rx) = mpsc::channel::<Message>();
    let send_connected = connected.clone();
    std::thread::spawn(move || {
        input_send_loop(sender, input_rx);
        send_connected.store(false, Ordering::Relaxed);
    });

    let mut stats_time = Instant::now();
    let mut stats_video: u64 = 0;

    loop {
        if !connected.load(Ordering::Relaxed) {
            return Ok(false);
        }

        let mut last_video = None;
        let mut last_tiles = None;
        let mut clipboard_msgs = Vec::new();
        while let Ok(msg) = frame_rx.try_recv() {
            match msg {
                Message::VideoFrame { .. } => last_video = Some(msg),
                Message::TileUpdate { .. } => last_tiles = Some(msg),
                Message::ClipboardSync(text) => clipboard_msgs.push(text),
                _ => {}
            }
        }

        // Apply clipboard from server
        for text in clipboard_msgs {
            if clipboard.on_remote_update(&text) {
                if let Some(ref mut ab) = arboard {
                    let _ = ab.set_text(&text);
                }
            }
        }

        if let Some(Message::VideoFrame { frame, .. }) = last_video {
            match h264_decoder.decode_frame(&frame.data) {
                Ok(rgb32) => {
                    display.update_full_frame(&rgb32);
                    stats_video += 1;
                }
                Err(e) => tracing::debug!("H.264 decode: {e}"),
            }
        }

        if let Some(Message::TileUpdate { tiles, .. }) = last_tiles {
            let mut decoded = Vec::with_capacity(tiles.len());
            for tile in &tiles {
                match tile_decoder.decode_tile(tile) {
                    Ok(dt) => decoded.push(dt),
                    Err(e) => tracing::debug!("tile decode: {e}"),
                }
            }
            let _ = display.update_tiles(&decoded);
        }

        if !display.present()? {
            return Ok(true); // window closed → quit
        }

        let Some(window) = display.window() else { continue };
        let events = input_capture.poll(window, |x, y| {
            display.map_mouse(x, y)
        });
        for event in events {
            let _ = input_tx.send(Message::Input(event));
        }

        // Poll local clipboard every 250ms
        if clipboard_poll.elapsed() >= Duration::from_millis(250) {
            clipboard_poll = Instant::now();
            if let Some(ref mut ab) = arboard {
                if let Ok(text) = ab.get_text() {
                    if let Some(changed) = clipboard.check_local_change(&text) {
                        let _ = input_tx.send(Message::ClipboardSync(changed));
                    }
                }
            }
        }

        if stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = stats_time.elapsed().as_secs_f64();
            tracing::info!(
                video_fps = format_args!("{:.1}", stats_video as f64 / elapsed),
                "stats"
            );
            stats_time = Instant::now();
            stats_video = 0;
        }

        std::thread::sleep(Duration::from_millis(1));
    }
}

fn recv_loop(mut receiver: Box<dyn MessageReceiver>, tx: mpsc::Sender<Message>) {
    while let Ok(msg) = receiver.recv_msg() {
        if tx.send(msg).is_err() { break; }
    }
}

fn input_send_loop(mut sender: Box<dyn MessageSender>, rx: mpsc::Receiver<Message>) {
    while let Ok(msg) = rx.recv() {
        if sender.send_msg(&msg).is_err() { break; }
    }
}
