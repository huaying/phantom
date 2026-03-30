mod decode_h264;
mod decode_zstd;
mod display_winit;
mod input_capture;
mod transport_quic;
mod transport_tcp;

use anyhow::{bail, Result};
use clap::Parser;
use phantom_core::clipboard::ClipboardTracker;
use phantom_core::crypto;
use phantom_core::decode::Decoder;
use phantom_core::encode::FrameDecoder;
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{WindowAttributes, WindowId};

#[derive(Parser)]
#[command(name = "phantom-client", about = "Phantom remote desktop client")]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:9900")]
    connect: String,
    #[arg(short, long)]
    key: Option<String>,
    #[arg(long)]
    no_encrypt: bool,
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

    let event_loop = EventLoop::new().map_err(|e| anyhow::anyhow!("create event loop: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        args_connect: args.connect,
        args_transport: args.transport,
        encryption_key,
        state: AppState::Disconnected,
        backoff: Duration::from_millis(500),
        last_connect_attempt: Instant::now() - Duration::from_secs(10),
    };

    event_loop.run_app(&mut app).map_err(|e| anyhow::anyhow!("{e}"))
}

#[allow(clippy::large_enum_variant)]
enum AppState {
    Disconnected,
    Connected(Session),
}

struct Session {
    display: display_winit::WinitDisplay,
    h264_decoder: decode_h264::OpenH264Decoder,
    tile_decoder: decode_zstd::ZstdDecoder,
    frame_rx: mpsc::Receiver<Message>,
    input_tx: mpsc::Sender<Message>,
    connected: Arc<AtomicBool>,
    cursor_pos: Option<PhysicalPosition<f64>>,
    modifiers: winit::event::Modifiers,
    clipboard: ClipboardTracker,
    arboard: Option<arboard::Clipboard>,
    clipboard_poll: Instant,
    stats_time: Instant,
    stats_video: u64,
}

struct App {
    args_connect: String,
    args_transport: String,
    encryption_key: Option<[u8; 32]>,
    state: AppState,
    backoff: Duration,
    last_connect_attempt: Instant,
}

impl App {
    fn try_connect(&mut self, event_loop: &ActiveEventLoop) {
        if self.last_connect_attempt.elapsed() < self.backoff {
            return;
        }
        self.last_connect_attempt = Instant::now();

        tracing::info!(addr = %self.args_connect, "connecting...");

        let result: Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> =
            if self.args_transport == "quic" {
                match transport_quic::QuicClientTransport::new()
                    .and_then(|q| q.connect(&self.args_connect))
                {
                    Ok((s, r)) => Ok((Box::new(s), Box::new(r))),
                    Err(e) => Err(e),
                }
            } else {
                match transport_tcp::TcpClientTransport::new(&self.args_connect).connect_tcp() {
                    Ok(conn) => {
                        if let Some(ref key) = self.encryption_key {
                            match conn.split_encrypted(key) {
                                Ok((s, r)) => Ok((Box::new(s) as _, Box::new(r) as _)),
                                Err(e) => Err(e),
                            }
                        } else {
                            match conn.split() {
                                Ok((s, r)) => Ok((Box::new(s) as _, Box::new(r) as _)),
                                Err(e) => Err(e),
                            }
                        }
                    }
                    Err(e) => Err(e),
                }
            };

        let (mut sender, mut receiver) = match result {
            Ok(pair) => {
                self.backoff = Duration::from_millis(500);
                pair
            }
            Err(e) => {
                tracing::warn!("connect failed: {e}, retry in {:.1}s", self.backoff.as_secs_f32());
                self.backoff = (self.backoff * 2).min(Duration::from_secs(10));
                return;
            }
        };

        // Read Hello
        let (width, height) = match receiver.recv_msg() {
            Ok(Message::Hello { width, height, .. }) if width > 0 && width <= 8192 && height > 0 && height <= 8192 => {
                tracing::info!(width, height, "connected");
                (width, height)
            }
            Ok(_) => { tracing::warn!("bad Hello"); return; }
            Err(e) => { tracing::warn!("handshake failed: {e}"); return; }
        };

        // Create window
        let win_size = display_winit::fit_window_size(width, height);
        let window = Rc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("Phantom Remote Desktop")
                        .with_inner_size(win_size),
                )
                .expect("create window"),
        );

        let display = match display_winit::WinitDisplay::new(window.clone(), width, height) {
            Ok(d) => d,
            Err(e) => { tracing::error!("display init failed: {e}"); return; }
        };

        let h264_decoder = match decode_h264::OpenH264Decoder::new(width, height) {
            Ok(d) => d,
            Err(e) => { tracing::error!("decoder init failed: {e}"); return; }
        };

        let connected = Arc::new(AtomicBool::new(true));

        let (frame_tx, frame_rx) = mpsc::channel();
        let recv_connected = connected.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = receiver.recv_msg() {
                if frame_tx.send(msg).is_err() { break; }
            }
            recv_connected.store(false, Ordering::Relaxed);
        });

        let (input_tx, input_rx) = mpsc::channel::<Message>();
        let send_connected = connected.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = input_rx.recv() {
                if sender.send_msg(&msg).is_err() { break; }
            }
            send_connected.store(false, Ordering::Relaxed);
        });

        self.state = AppState::Connected(Session {
            display,
            h264_decoder,
            tile_decoder: decode_zstd::ZstdDecoder::new(),
            frame_rx,
            input_tx,
            connected,
            cursor_pos: None,
            modifiers: winit::event::Modifiers::default(),
            clipboard: ClipboardTracker::default(),
            arboard: arboard::Clipboard::new().ok(),
            clipboard_poll: Instant::now(),
            stats_time: Instant::now(),
            stats_video: 0,
        });
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            AppState::Disconnected => {
                self.try_connect(event_loop);
                // Small sleep to not busy-loop while disconnected
                std::thread::sleep(Duration::from_millis(16));
            }
            AppState::Connected(session) => {
                // Check disconnect
                if !session.connected.load(Ordering::Relaxed) {
                    tracing::info!("disconnected, will reconnect...");
                    self.state = AppState::Disconnected;
                    return;
                }

                // Process received frames — decode every VideoFrame sequentially
                let mut last_tiles = None;
                let mut clipboard_msgs = Vec::new();
                while let Ok(msg) = session.frame_rx.try_recv() {
                    match msg {
                        Message::VideoFrame { frame, .. } => {
                            if let Ok(rgb32) = session.h264_decoder.decode_frame(&frame.data) {
                                session.display.update_full_frame(&rgb32);
                                session.stats_video += 1;
                            }
                        }
                        Message::TileUpdate { .. } => last_tiles = Some(msg),
                        Message::ClipboardSync(t) => clipboard_msgs.push(t),
                        _ => {}
                    }
                }

                // Clipboard from server
                for text in clipboard_msgs {
                    if session.clipboard.on_remote_update(&text) {
                        if let Some(ref mut ab) = session.arboard {
                            let _ = ab.set_text(&text);
                        }
                    }
                }

                // Clipboard poll (local → server)
                if session.clipboard_poll.elapsed() >= Duration::from_millis(250) {
                    session.clipboard_poll = Instant::now();
                    if let Some(ref mut ab) = session.arboard {
                        if let Ok(text) = ab.get_text() {
                            if let Some(changed) = session.clipboard.check_local_change(&text) {
                                let _ = session.input_tx.send(Message::ClipboardSync(changed));
                            }
                        }
                    }
                }
                if let Some(Message::TileUpdate { tiles, .. }) = last_tiles {
                    let mut decoded = Vec::with_capacity(tiles.len());
                    for tile in tiles.iter() {
                        if let Ok(dt) = session.tile_decoder.decode_tile(tile) {
                            decoded.push(dt);
                        }
                    }
                    session.display.update_tiles(&decoded);
                }

                // Present
                let _ = session.display.present(session.cursor_pos);

                // Stats
                if session.stats_time.elapsed() >= Duration::from_secs(5) {
                    let elapsed = session.stats_time.elapsed().as_secs_f64();
                    tracing::info!(
                        video_fps = format_args!("{:.1}", session.stats_video as f64 / elapsed),
                        "stats"
                    );
                    session.stats_time = Instant::now();
                    session.stats_video = 0;
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let AppState::Connected(session) = &mut self.state else { return };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(mods) => {
                session.modifiers = mods;
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Intercept Cmd+V (Mac) / Ctrl+V → paste clipboard as typed text
                let mods = session.modifiers.state();
                let is_paste = key_event.state == winit::event::ElementState::Pressed
                    && !key_event.repeat
                    && matches!(
                        key_event.physical_key,
                        winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyV)
                    )
                    && (mods.super_key() || mods.control_key());

                if is_paste {
                    if let Some(ref mut ab) = session.arboard {
                        if let Ok(text) = ab.get_text() {
                            if !text.is_empty() {
                                let _ = session.input_tx.send(Message::PasteText(text));
                                return; // eat the V key
                            }
                        }
                    }
                }

                if let Some(input) = input_capture::key_event(&key_event.physical_key, key_event.state) {
                    let _ = session.input_tx.send(Message::Input(input));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                session.cursor_pos = Some(position);
                let (sx, sy) = session.display.map_to_server(position);
                let _ = session.input_tx.send(Message::Input(
                    input_capture::mouse_move_event(sx, sy),
                ));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(input) = input_capture::mouse_button_event(button, state) {
                    let _ = session.input_tx.send(Message::Input(input));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(input) = input_capture::scroll_event(delta) {
                    let _ = session.input_tx.send(Message::Input(input));
                }
            }
            _ => {}
        }
    }
}
