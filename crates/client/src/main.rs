//! Phantom remote desktop client (native, using winit + wgpu).
//!
//! Connects to a Phantom server via TCP or QUIC, decodes H.264 video,
//! renders to a window, and forwards keyboard/mouse input back to the
//! server. Supports encrypted connections, audio playback, clipboard
//! sync, and bidirectional file transfer.

#[cfg(feature = "audio")]
mod audio_playback;
mod decode_h264;
#[cfg(target_os = "macos")]
mod decode_videotoolbox;
mod decode_zstd;
mod display_winit;
mod file_transfer;
mod input_capture;
mod transport_quic;
mod transport_tcp;

use anyhow::{bail, Result};
use clap::Parser;
use phantom_core::clipboard::ClipboardTracker;
use phantom_core::crypto;
use phantom_core::decode::Decoder;
use phantom_core::input::{InputEvent, KeyCode};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Result of a successful connection attempt: sender, receiver, and optional TCP shutdown handle.
type ConnectResult = Result<(
    Box<dyn MessageSender>,
    Box<dyn MessageReceiver>,
    Option<transport_tcp::TcpShutdownHandle>,
)>;
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
    /// Video decoder: auto (default), openh264 (CPU), videotoolbox (macOS GPU).
    #[arg(long, default_value = "auto")]
    decoder: String,
    /// Send a file to the server after connecting.
    #[arg(long)]
    send_file: Option<String>,
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
        args_decoder: args.decoder,
        encryption_key,
        state: AppState::Disconnected,
        backoff: Duration::from_millis(500),
        last_connect_attempt: Instant::now() - Duration::from_secs(10),
        send_file: args.send_file,
        send_file_initiated: false,
    };

    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[allow(clippy::large_enum_variant)]
enum AppState {
    Disconnected,
    Connected(Session),
}

struct Session {
    display: display_winit::WinitDisplay,
    h264_decoder: Box<dyn phantom_core::encode::FrameDecoder>,
    tile_decoder: decode_zstd::ZstdDecoder,
    frame_rx: mpsc::Receiver<Message>,
    input_tx: mpsc::Sender<Message>,
    connected: Arc<AtomicBool>,
    /// Set to true when server sends a Disconnect message (don't reconnect).
    server_kicked: Arc<AtomicBool>,
    /// Shutdown handle to close the TCP connection and unblock the recv thread.
    shutdown_handle: Option<transport_tcp::TcpShutdownHandle>,
    cursor_pos: Option<PhysicalPosition<f64>>,
    last_sent_mouse: (i32, i32),
    modifiers: winit::event::Modifiers,
    clipboard: ClipboardTracker,
    arboard: Option<arboard::Clipboard>,
    clipboard_poll: Instant,
    stats_time: Instant,
    stats_video: u64,
    stats_decode_ms: f64,
    /// Send Opus packets to audio playback thread (None if no audio).
    audio_tx: Option<mpsc::SyncSender<Vec<u8>>>,
    /// File transfer handler.
    file_xfer: file_transfer::ClientFileTransfer,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Shutdown the TCP connection to unblock the recv thread's blocking read.
        if let Some(ref handle) = self.shutdown_handle {
            handle.shutdown();
        }
    }
}

struct App {
    args_connect: String,
    args_transport: String,
    args_decoder: String,
    encryption_key: Option<[u8; 32]>,
    state: AppState,
    backoff: Duration,
    last_connect_attempt: Instant,
    send_file: Option<String>,
    send_file_initiated: bool,
}

impl App {
    fn try_connect(&mut self, event_loop: &ActiveEventLoop) {
        if self.last_connect_attempt.elapsed() < self.backoff {
            return;
        }
        self.last_connect_attempt = Instant::now();

        tracing::info!(addr = %self.args_connect, "connecting...");

        let result: ConnectResult = if self.args_transport == "quic" {
            match transport_quic::QuicClientTransport::new()
                .and_then(|q| q.connect(&self.args_connect))
            {
                Ok((s, r)) => Ok((Box::new(s), Box::new(r), None)),
                Err(e) => Err(e),
            }
        } else {
            match transport_tcp::TcpClientTransport::new(&self.args_connect).connect_tcp() {
                Ok(conn) => {
                    let shutdown_handle = conn.shutdown_handle().ok();
                    if let Some(ref key) = self.encryption_key {
                        match conn.split_encrypted(key) {
                            Ok((s, r)) => Ok((Box::new(s) as _, Box::new(r) as _, shutdown_handle)),
                            Err(e) => Err(e),
                        }
                    } else {
                        match conn.split() {
                            Ok((s, r)) => Ok((Box::new(s) as _, Box::new(r) as _, shutdown_handle)),
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            }
        };

        let (mut sender, mut receiver, shutdown_handle) = match result {
            Ok(pair) => {
                self.backoff = Duration::from_millis(500);
                pair
            }
            Err(e) => {
                tracing::warn!(
                    "connect failed: {e}, retry in {:.1}s",
                    self.backoff.as_secs_f32()
                );
                self.backoff = (self.backoff * 2).min(Duration::from_secs(10));
                return;
            }
        };

        // Read Hello and check protocol version
        let (width, height, server_audio) = match receiver.recv_msg() {
            Ok(Message::Hello {
                width,
                height,
                audio,
                protocol_version,
                ..
            }) if width > 0 && width <= 8192 && height > 0 && height <= 8192 => {
                if protocol_version < phantom_core::protocol::MIN_PROTOCOL_VERSION {
                    tracing::error!(
                        server_version = protocol_version,
                        min = phantom_core::protocol::MIN_PROTOCOL_VERSION,
                        "server protocol too old, please upgrade the server"
                    );
                    return;
                }
                if protocol_version > phantom_core::protocol::PROTOCOL_VERSION {
                    tracing::warn!(
                        server_version = protocol_version,
                        client_version = phantom_core::protocol::PROTOCOL_VERSION,
                        "server is newer, some features may not work"
                    );
                }
                tracing::info!(width, height, audio, protocol_version, "connected");
                (width, height, audio)
            }
            Ok(_) => {
                tracing::warn!("bad Hello");
                return;
            }
            Err(e) => {
                tracing::warn!("handshake failed: {e}");
                return;
            }
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
            Err(e) => {
                tracing::error!("display init failed: {e}");
                return;
            }
        };

        let _decoder_name = &self.args_decoder;
        let h264_decoder: Box<dyn phantom_core::encode::FrameDecoder> = {
            #[cfg(target_os = "macos")]
            if _decoder_name == "auto" || _decoder_name == "videotoolbox" {
                match decode_videotoolbox::VideoToolboxDecoder::new(width, height) {
                    Ok(d) => {
                        tracing::info!("using VideoToolbox hardware decoder");
                        Box::new(d)
                    }
                    Err(e) => {
                        tracing::warn!("VideoToolbox init failed ({e}), falling back to OpenH264");
                        match decode_h264::OpenH264Decoder::new(width, height) {
                            Ok(d) => Box::new(d),
                            Err(e) => {
                                tracing::error!("decoder init failed: {e}");
                                return;
                            }
                        }
                    }
                }
            } else {
                match decode_h264::OpenH264Decoder::new(width, height) {
                    Ok(d) => {
                        tracing::info!("using OpenH264 software decoder");
                        Box::new(d)
                    }
                    Err(e) => {
                        tracing::error!("decoder init failed: {e}");
                        return;
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            match decode_h264::OpenH264Decoder::new(width, height) {
                Ok(d) => Box::new(d),
                Err(e) => {
                    tracing::error!("decoder init failed: {e}");
                    return;
                }
            }
        };

        let connected = Arc::new(AtomicBool::new(true));
        let server_kicked = Arc::new(AtomicBool::new(false));

        // Start audio playback if server supports it
        #[cfg(feature = "audio")]
        let audio_tx: Option<mpsc::SyncSender<Vec<u8>>> = if server_audio {
            match audio_playback::start_playback(48000, 2) {
                Ok(tx) => Some(tx),
                Err(e) => {
                    tracing::warn!("audio playback init failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(feature = "audio"))]
        let audio_tx: Option<mpsc::SyncSender<Vec<u8>>> = {
            let _ = server_audio;
            None
        };

        let (frame_tx, frame_rx) = mpsc::channel();
        let recv_connected = connected.clone();
        let recv_kicked = server_kicked.clone();
        std::thread::Builder::new()
            .name("client-recv".into())
            .spawn(move || {
                while let Ok(msg) = receiver.recv_msg() {
                    if let Message::Disconnect { reason } = &msg {
                        tracing::info!("server disconnected us: {reason}");
                        recv_kicked.store(true, Ordering::Relaxed);
                        recv_connected.store(false, Ordering::Relaxed);
                        break;
                    }
                    if frame_tx.send(msg).is_err() {
                        break;
                    }
                }
                recv_connected.store(false, Ordering::Relaxed);
                tracing::debug!("recv thread exiting");
            })
            .expect("spawn recv thread");

        let (input_tx, input_rx) = mpsc::channel::<Message>();
        let send_connected = connected.clone();
        std::thread::Builder::new()
            .name("client-send".into())
            .spawn(move || {
                while let Ok(msg) = input_rx.recv() {
                    if sender.send_msg(&msg).is_err() {
                        break;
                    }
                }
                send_connected.store(false, Ordering::Relaxed);
                tracing::debug!("send thread exiting");
            })
            .expect("spawn send thread");

        self.state = AppState::Connected(Session {
            display,
            h264_decoder,
            tile_decoder: decode_zstd::ZstdDecoder::new(),
            frame_rx,
            input_tx,
            connected,
            server_kicked,
            shutdown_handle,
            cursor_pos: None,
            last_sent_mouse: (-1, -1),
            modifiers: winit::event::Modifiers::default(),
            clipboard: ClipboardTracker::default(),
            arboard: arboard::Clipboard::new().ok(),
            clipboard_poll: Instant::now(),
            stats_time: Instant::now(),
            stats_video: 0,
            stats_decode_ms: 0.0,
            audio_tx,
            file_xfer: file_transfer::ClientFileTransfer::new(),
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
                    if session.server_kicked.load(Ordering::Relaxed) {
                        tracing::info!("server replaced this session, exiting");
                        event_loop.exit();
                        return;
                    }
                    tracing::info!("disconnected, will reconnect...");
                    self.state = AppState::Disconnected;
                    return;
                }

                // Initiate --send-file if specified (once per connection)
                if !self.send_file_initiated {
                    if let Some(ref path_str) = self.send_file {
                        let path = std::path::Path::new(path_str);
                        match session.file_xfer.initiate_send(path) {
                            Ok((_id, offer)) => {
                                let _ = session.input_tx.send(offer);
                            }
                            Err(e) => {
                                tracing::error!("--send-file failed: {e}");
                            }
                        }
                    }
                    self.send_file_initiated = true;
                }

                // Process received frames — decode every VideoFrame to maintain
                // decoder state, but only render the last decoded result.
                let mut last_decoded = None;
                let mut last_tiles = None;
                let mut clipboard_msgs = Vec::new();
                while let Ok(msg) = session.frame_rx.try_recv() {
                    match msg {
                        Message::VideoFrame { frame, .. } => {
                            let decode_start = std::time::Instant::now();
                            match session.h264_decoder.decode_frame(&frame.data) {
                                Ok(rgb32) => {
                                    let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
                                    session.stats_decode_ms += decode_ms;
                                    last_decoded = Some(rgb32);
                                    session.stats_video += 1;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        size = frame.data.len(),
                                        keyframe = frame.is_keyframe,
                                        "decode failed: {e}"
                                    );
                                }
                            }
                        }
                        Message::TileUpdate { .. } => last_tiles = Some(msg),
                        Message::ClipboardSync(t) => clipboard_msgs.push(t),
                        Message::AudioFrame { data, .. } => {
                            if let Some(ref audio_tx) = session.audio_tx {
                                let _ = audio_tx.try_send(data);
                            }
                        }
                        Message::Ping => {
                            let _ = session.input_tx.send(Message::Pong);
                        }
                        Message::FileOffer {
                            transfer_id,
                            name,
                            size,
                        } => match session.file_xfer.on_file_offer(transfer_id, &name, size) {
                            Ok(reply) => {
                                let _ = session.input_tx.send(reply);
                            }
                            Err(e) => {
                                tracing::error!(transfer_id, "failed to accept file: {e}");
                                let _ = session.input_tx.send(Message::FileCancel {
                                    transfer_id,
                                    reason: format!("{e}"),
                                });
                            }
                        },
                        Message::FileAccept { transfer_id } => {
                            session.file_xfer.on_file_accept(transfer_id);
                        }
                        Message::FileCancel {
                            transfer_id,
                            reason,
                        } => {
                            session.file_xfer.on_file_cancel(transfer_id, &reason);
                        }
                        Message::FileChunk {
                            transfer_id,
                            offset,
                            data,
                        } => {
                            if let Err(e) =
                                session.file_xfer.on_file_chunk(transfer_id, offset, &data)
                            {
                                tracing::error!(transfer_id, "file chunk error: {e}");
                            }
                        }
                        Message::FileDone {
                            transfer_id,
                            sha256,
                        } => {
                            if let Err(e) = session.file_xfer.on_file_done(transfer_id, &sha256) {
                                tracing::error!(transfer_id, "file done error: {e}");
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(rgb32) = last_decoded {
                    session.display.update_full_frame(&rgb32);
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

                // Drain file transfer outbound messages
                for msg in session.file_xfer.drain_send_events() {
                    let _ = session.input_tx.send(msg);
                }

                // Stats
                if session.stats_time.elapsed() >= Duration::from_secs(5) {
                    let elapsed = session.stats_time.elapsed().as_secs_f64();
                    let avg_decode = if session.stats_video > 0 {
                        session.stats_decode_ms / session.stats_video as f64
                    } else {
                        0.0
                    };
                    tracing::info!(
                        video_fps = format_args!("{:.1}", session.stats_video as f64 / elapsed),
                        decode_avg_ms = format_args!("{:.2}", avg_decode),
                        "stats"
                    );
                    session.stats_time = Instant::now();
                    session.stats_video = 0;
                    session.stats_decode_ms = 0.0;
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
        let AppState::Connected(session) = &mut self.state else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(mods) => {
                session.modifiers = mods;
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
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

                if let Some(input) =
                    input_capture::key_event(&key_event.physical_key, key_event.state)
                {
                    let _ = session.input_tx.send(Message::Input(input));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                session.cursor_pos = Some(position);
                let (sx, sy) = session.display.map_to_server(position);
                // Only send if position actually changed (filters trackpad noise)
                let (lx, ly) = session.last_sent_mouse;
                if sx != lx || sy != ly {
                    session.last_sent_mouse = (sx, sy);
                    let _ = session
                        .input_tx
                        .send(Message::Input(input_capture::mouse_move_event(sx, sy)));
                }
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
            WindowEvent::Focused(false) => {
                // Release all modifiers when window loses focus to prevent stuck keys.
                for key in [
                    KeyCode::LeftShift,
                    KeyCode::RightShift,
                    KeyCode::LeftCtrl,
                    KeyCode::RightCtrl,
                    KeyCode::LeftAlt,
                    KeyCode::RightAlt,
                ] {
                    let _ = session.input_tx.send(Message::Input(InputEvent::Key {
                        key,
                        pressed: false,
                    }));
                }
            }
            _ => {}
        }
    }
}
