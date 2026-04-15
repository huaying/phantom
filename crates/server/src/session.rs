//! Shared session logic: input dispatch, clipboard sync, keepalive, stats,
//! congestion control, and frame pacing.
//!
//! The three capture+encode pipelines (CPU, Linux GPU, Windows DXGI) use
//! `SessionRunner` to avoid duplicating the ~80% of session code that is
//! transport/input/clipboard plumbing.

use crate::encode_zstd::ZstdEncoder;
use crate::input_injector::InputInjector;
use anyhow::Result;
use phantom_core::clipboard::ClipboardTracker;
use phantom_core::encode::{EncodedFrame, Encoder, FrameEncoder};
use phantom_core::frame::Frame;
use phantom_core::input::InputEvent;
#[cfg(feature = "audio")]
use phantom_core::protocol::AudioCodec;
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Input forwarding (for service mode IPC) ─────────────────────────────────

/// Trait for forwarding input events to a remote agent instead of local injection.
/// Used by Windows Service mode to send input over IPC to the agent process.
pub trait InputForwarder: Send {
    fn forward_input(&self, event: &InputEvent) -> Result<()>;
}

// ── Inbound events from the network receive thread ──────────────────────────

pub enum InboundEvent {
    Input(InputEvent),
    Clipboard(String),
    PasteText(String),
    Pong,
    FileOffer {
        transfer_id: u64,
        name: String,
        size: u64,
    },
    FileAccept {
        transfer_id: u64,
    },
    FileCancel {
        transfer_id: u64,
        reason: String,
    },
    FileChunk {
        transfer_id: u64,
        offset: u64,
        data: Vec<u8>,
    },
    FileDone {
        transfer_id: u64,
        sha256: [u8; 32],
    },
    /// Client wants to resume a previous session — force keyframe.
    Resume {
        session_token: Vec<u8>,
        last_sequence: u64,
    },
    Disconnected,
}

/// Spawn a background thread that reads messages from `receiver` and forwards
/// them as `InboundEvent`s to the returned channel.
pub fn spawn_receive_thread(
    mut receiver: Box<dyn MessageReceiver>,
) -> mpsc::Receiver<InboundEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("session-recv".into())
        .spawn(move || loop {
            match receiver.recv_msg() {
                Ok(Message::Input(event)) => {
                    let _ = tx.send(InboundEvent::Input(event));
                }
                Ok(Message::ClipboardSync(text)) => {
                    let _ = tx.send(InboundEvent::Clipboard(text));
                }
                Ok(Message::PasteText(text)) => {
                    let _ = tx.send(InboundEvent::PasteText(text));
                }
                Ok(Message::Pong) => {
                    let _ = tx.send(InboundEvent::Pong);
                }
                Ok(Message::FileOffer {
                    transfer_id,
                    name,
                    size,
                }) => {
                    let _ = tx.send(InboundEvent::FileOffer {
                        transfer_id,
                        name,
                        size,
                    });
                }
                Ok(Message::FileAccept { transfer_id }) => {
                    let _ = tx.send(InboundEvent::FileAccept { transfer_id });
                }
                Ok(Message::FileCancel {
                    transfer_id,
                    reason,
                }) => {
                    let _ = tx.send(InboundEvent::FileCancel {
                        transfer_id,
                        reason,
                    });
                }
                Ok(Message::FileChunk {
                    transfer_id,
                    offset,
                    data,
                }) => {
                    let _ = tx.send(InboundEvent::FileChunk {
                        transfer_id,
                        offset,
                        data,
                    });
                }
                Ok(Message::FileDone {
                    transfer_id,
                    sha256,
                }) => {
                    let _ = tx.send(InboundEvent::FileDone {
                        transfer_id,
                        sha256,
                    });
                }
                Ok(Message::Resume {
                    session_token,
                    last_sequence,
                }) => {
                    let _ = tx.send(InboundEvent::Resume {
                        session_token,
                        last_sequence,
                    });
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(InboundEvent::Disconnected);
                    break;
                }
            }
        })
        .expect("spawn receive thread");
    rx
}

// ── Quality state (lossless refinement after idle period) ───────────────────

pub struct QualityState {
    last_motion: Instant,
    lossless_sent: bool,
    delay: Duration,
}

impl QualityState {
    pub fn new(delay: Duration) -> Self {
        Self {
            last_motion: Instant::now(),
            lossless_sent: false,
            delay,
        }
    }
    pub fn on_motion(&mut self) {
        self.last_motion = Instant::now();
        self.lossless_sent = false;
    }
    pub fn should_send_lossless(&self) -> bool {
        !self.lossless_sent && self.last_motion.elapsed() >= self.delay
    }
    pub fn mark_lossless_sent(&mut self) {
        self.lossless_sent = true;
    }
}

// ── Congestion tracker ──────────────────────────────────────────────────────

pub struct CongestionTracker {
    frame_interval: Duration,
    slow_frames: u32,
    pub(crate) skip_ratio: u32,
    frame_counter: u64,
}

impl CongestionTracker {
    pub fn new(frame_interval: Duration) -> Self {
        Self {
            frame_interval,
            slow_frames: 0,
            skip_ratio: 1,
            frame_counter: 0,
        }
    }

    pub fn should_skip_frame(&mut self) -> bool {
        self.frame_counter += 1;
        if self.skip_ratio <= 1 {
            return false;
        }
        !self.frame_counter.is_multiple_of(self.skip_ratio as u64)
    }

    pub fn on_frame_sent(&mut self, send_duration: Duration) {
        if send_duration > self.frame_interval * 2 {
            self.slow_frames += 1;
            if self.slow_frames > 3 && self.skip_ratio < 4 {
                self.skip_ratio += 1;
                tracing::info!(
                    skip_ratio = self.skip_ratio,
                    "reducing frame rate (congestion)"
                );
            }
        } else {
            if self.slow_frames > 0 {
                self.slow_frames -= 1;
            }
            if self.slow_frames == 0 && self.skip_ratio > 1 {
                self.skip_ratio -= 1;
                tracing::info!(
                    skip_ratio = self.skip_ratio,
                    "increasing frame rate (recovered)"
                );
            }
        }
    }
}

// ── Adaptive bitrate controller ─────────────────────────────────────────────

/// Dynamically adjusts encoder bitrate based on RTT and congestion.
///
/// Strategy:
/// - RTT > 100ms or congested → decrease bitrate (×0.7)
/// - RTT < 50ms and stable for 10s → increase bitrate (×1.2)
/// - Clamped to \[min_kbps, max_kbps\]
/// - Changes at most once per 5s (hysteresis)
pub struct AdaptiveBitrate {
    current_kbps: u32,
    min_kbps: u32,
    max_kbps: u32,
    last_change: Instant,
    stable_since: Option<Instant>,
    /// Baseline RTT (minimum observed). Used to detect congestion vs fixed latency.
    baseline_rtt_ms: Option<f64>,
}

impl AdaptiveBitrate {
    pub fn new(initial_kbps: u32) -> Self {
        Self {
            current_kbps: initial_kbps,
            min_kbps: 1500, // Don't go below 1500kbps (was 500 — too low, causes blur)
            max_kbps: initial_kbps * 3,
            last_change: Instant::now(),
            stable_since: None,
            baseline_rtt_ms: None,
        }
    }

    /// Evaluate whether to change bitrate. Returns Some(new_kbps) if a change is needed.
    ///
    /// Key insight: only decrease on CONGESTION (RTT increasing above baseline),
    /// not on fixed high latency (e.g. 100ms to a distant server).
    pub fn evaluate(&mut self, rtt_us: Option<u64>, congestion_skip_ratio: u32) -> Option<u32> {
        // Don't change more than once every 5s
        if self.last_change.elapsed() < Duration::from_secs(5) {
            return None;
        }

        let rtt_ms = rtt_us.map(|us| us as f64 / 1000.0).unwrap_or(0.0);
        if rtt_ms <= 0.0 {
            return None;
        }

        // Track baseline RTT (minimum observed = fixed network latency)
        match self.baseline_rtt_ms {
            None => self.baseline_rtt_ms = Some(rtt_ms),
            Some(ref mut base) => {
                // Slowly adapt baseline upward (EMA) but snap down immediately
                if rtt_ms < *base {
                    *base = rtt_ms;
                } else {
                    *base = *base * 0.95 + rtt_ms * 0.05;
                }
            }
        }
        let baseline = self.baseline_rtt_ms.unwrap_or(rtt_ms);

        // Congestion = RTT significantly above baseline (>50% increase)
        let congested = rtt_ms > baseline * 1.5 && rtt_ms > baseline + 30.0;

        // Decrease: actual congestion or frame skip
        if congested || congestion_skip_ratio > 1 {
            let new = ((self.current_kbps as f64 * 0.7) as u32).max(self.min_kbps);
            if new < self.current_kbps {
                self.stable_since = None;
                return Some(new);
            }
        }

        // Increase: RTT near baseline (not congested) and stable for 10s
        if !congested && congestion_skip_ratio <= 1 {
            if self.stable_since.is_none() {
                self.stable_since = Some(Instant::now());
            }
            if let Some(since) = self.stable_since {
                if since.elapsed() >= Duration::from_secs(10) && self.current_kbps < self.max_kbps {
                    let new = ((self.current_kbps as f64 * 1.2) as u32).min(self.max_kbps);
                    if new > self.current_kbps {
                        self.stable_since = Some(Instant::now());
                        return Some(new);
                    }
                }
            }
        } else {
            self.stable_since = None;
        }

        None
    }

    /// Apply a bitrate change. Call after set_bitrate_kbps succeeds on the encoder.
    pub fn apply(&mut self, new_kbps: u32) {
        tracing::info!(
            from = self.current_kbps,
            to = new_kbps,
            "adaptive bitrate change"
        );
        self.current_kbps = new_kbps;
        self.last_change = Instant::now();
    }

    #[allow(dead_code)]
    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }
}

// ── SessionRunner: shared session plumbing ──────────────────────────────────

pub struct SessionRunner {
    pub sender: Box<dyn MessageSender>,
    /// Separate audio sender (independent WebSocket). Falls back to main sender.
    pub audio_sender: Option<Box<dyn MessageSender>>,
    /// Receiver for audio WS connections (set from main.rs).
    pub audio_ws_rx: Option<mpsc::Receiver<crate::transport_ws::WsSender>>,
    pub event_rx: mpsc::Receiver<InboundEvent>,
    pub injector: Option<InputInjector>,
    pub clipboard: ClipboardTracker,
    pub arboard: Option<arboard::Clipboard>,
    pub clipboard_poll: Instant,
    pub sequence: u64,
    pub stats_time: Instant,
    pub stats_frames: u64,
    pub stats_bytes: u64,
    pub keepalive_time: Instant,
    pub had_input: bool,
    pub frame_interval: Duration,
    pub last_keyframe_time: Instant,
    /// Last time we sent a Ping (for RTT measurement).
    pub ping_sent_at: Option<Instant>,
    /// Smoothed round-trip time (exponential moving average).
    pub rtt_us: Option<u64>,
    /// Accumulated encode time for stats period.
    pub stats_encode_us: u64,
    /// Set to true by the accept loop when a new client connects.
    /// The session loop checks this and exits cleanly.
    pub cancel: Arc<AtomicBool>,
    /// Audio capture thread + receiver (None if audio feature disabled or init failed).
    #[cfg(feature = "audio")]
    pub audio_rx: Option<mpsc::Receiver<crate::audio_capture::AudioChunk>>,
    #[cfg(feature = "audio")]
    pub _audio_capture: Option<crate::audio_capture::AudioCapture>,
    /// File transfer handler.
    pub file_transfer: crate::file_transfer::ServerFileTransfer,
    /// Session token for reconnect validation.
    pub session_token: Vec<u8>,
    /// Optional input forwarder (e.g. IPC to agent in service mode).
    /// When set, input events are forwarded instead of locally injected.
    pub input_forwarder: Option<Box<dyn InputForwarder>>,
}

impl SessionRunner {
    /// Create a new session runner, spawn the receive thread, send Hello, and
    /// hide the remote cursor. Optionally starts audio capture.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sender: Box<dyn MessageSender>,
        receiver: Box<dyn MessageReceiver>,
        width: u32,
        height: u32,
        frame_interval: Duration,
        cancel: Arc<AtomicBool>,
        video_codec: phantom_core::encode::VideoCodec,
        is_resume: bool,
    ) -> Result<Self> {
        let event_rx = spawn_receive_thread(receiver);
        let injector = InputInjector::new().ok();
        let clipboard = ClipboardTracker::new();
        let arboard = arboard::Clipboard::new().ok();

        // Start audio capture (best-effort: don't fail session if audio unavailable)
        #[cfg(feature = "audio")]
        let (audio_capture, audio_rx) = match crate::audio_capture::AudioCapture::start() {
            Ok((capture, rx)) => (Some(capture), Some(rx)),
            Err(e) => {
                tracing::warn!("audio capture unavailable: {e}");
                (None, None)
            }
        };

        #[cfg(feature = "audio")]
        let has_audio = audio_rx.is_some();
        #[cfg(not(feature = "audio"))]
        let has_audio = false;

        let mut runner = Self {
            sender,
            audio_sender: None,
            audio_ws_rx: None,
            event_rx,
            injector,
            clipboard,
            arboard,
            clipboard_poll: Instant::now(),
            sequence: 0,
            stats_time: Instant::now(),
            stats_frames: 0,
            stats_bytes: 0,
            keepalive_time: Instant::now(),
            had_input: false,
            frame_interval,
            last_keyframe_time: Instant::now() - Duration::from_secs(10),
            ping_sent_at: None,
            rtt_us: None,
            stats_encode_us: 0,
            cancel,
            #[cfg(feature = "audio")]
            audio_rx,
            #[cfg(feature = "audio")]
            _audio_capture: audio_capture,
            file_transfer: crate::file_transfer::ServerFileTransfer::new(),
            session_token: Vec::new(),
            input_forwarder: None,
        };

        // Generate session token for reconnect
        let session_token: Vec<u8> = {
            use ring::rand::SecureRandom;
            let rng = ring::rand::SystemRandom::new();
            let mut token = vec![0u8; 32];
            rng.fill(&mut token).expect("RNG failed");
            token
        };

        if is_resume {
            // Resume: send ResumeOk instead of Hello, same session token
            runner.sender.send_msg(&Message::ResumeOk)?;
            tracing::info!(width, height, "session resumed");
            // Force keyframe on first frame (reset last_keyframe_time to epoch)
            runner.last_keyframe_time = Instant::now() - Duration::from_secs(3600);
        } else {
            runner.sender.send_msg(&Message::Hello {
                width,
                height,
                format: phantom_core::frame::PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: has_audio,
                video_codec,
                session_token: session_token.clone(),
            })?;
            tracing::info!(width, height, audio = has_audio, "session started");
        }

        runner.session_token = session_token;

        hide_remote_cursor();

        Ok(runner)
    }

    /// Check if the session has been cancelled (new client replaced us).
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Send a Disconnect message to the client (best-effort, ignores errors).
    pub fn send_disconnect(&mut self, reason: &str) {
        let _ = self.sender.send_msg(&Message::Disconnect {
            reason: reason.to_string(),
        });
    }

    /// Check if cancelled and, if so, send Disconnect before bailing.
    pub fn check_cancelled(&mut self) -> Result<()> {
        if self.is_cancelled() {
            self.send_disconnect("replaced by new client");
            anyhow::bail!("replaced by new client");
        }
        Ok(())
    }

    /// Drain all pending inbound events (input, clipboard, paste).
    /// Returns `Err` if the client disconnected.
    pub fn pump_events(&mut self) -> Result<()> {
        // Check for audio WS upgrade
        if self.audio_sender.is_none() {
            if let Some(ref rx) = self.audio_ws_rx {
                if let Ok(sender) = rx.try_recv() {
                    self.set_audio_sender(Box::new(sender));
                }
            }
        }

        loop {
            match self.event_rx.try_recv() {
                Ok(InboundEvent::Input(event)) => {
                    // If we have an input forwarder (e.g. IPC to agent), use it.
                    // Otherwise inject locally (console mode).
                    if let Some(ref fwd) = self.input_forwarder {
                        let _ = fwd.forward_input(&event);
                    } else if let Some(ref mut inj) = self.injector {
                        let _ = inj.inject(&event);
                    }
                    self.had_input = true;
                }
                Ok(InboundEvent::Clipboard(text)) => {
                    if self.clipboard.on_remote_update(&text) {
                        if let Some(ref mut ab) = self.arboard {
                            let _ = ab.set_text(&text);
                        }
                    }
                }
                Ok(InboundEvent::PasteText(text)) => {
                    if let Some(ref mut ab) = self.arboard {
                        let _ = ab.set_text(&text);
                    }
                    self.clipboard.on_remote_update(&text);
                    // In service mode (input_forwarder set), paste forwarding is not yet
                    // supported — would need a PasteText IPC message type. For now,
                    // paste only works in console mode via local injection.
                    if let Some(ref mut inj) = self.injector {
                        let _ = inj.type_text(&text);
                        self.had_input = true;
                    }
                    tracing::debug!("paste: {} chars", text.len());
                }
                Ok(InboundEvent::Pong) => {
                    if let Some(sent_at) = self.ping_sent_at.take() {
                        let rtt = sent_at.elapsed().as_micros() as u64;
                        // Exponential moving average (α = 0.2)
                        self.rtt_us = Some(match self.rtt_us {
                            Some(prev) => (prev * 4 + rtt) / 5,
                            None => rtt,
                        });
                    }
                }
                Ok(InboundEvent::FileOffer {
                    transfer_id,
                    name,
                    size,
                }) => match self.file_transfer.on_file_offer(transfer_id, &name, size) {
                    Ok(reply) => {
                        let _ = self.sender.send_msg(&reply);
                    }
                    Err(e) => {
                        tracing::error!(transfer_id, "failed to accept file offer: {e}");
                        let _ = self.sender.send_msg(&Message::FileCancel {
                            transfer_id,
                            reason: format!("{e}"),
                        });
                    }
                },
                Ok(InboundEvent::FileAccept { transfer_id }) => {
                    self.file_transfer.on_file_accept(transfer_id);
                }
                Ok(InboundEvent::FileCancel {
                    transfer_id,
                    reason,
                }) => {
                    self.file_transfer.on_file_cancel(transfer_id, &reason);
                }
                Ok(InboundEvent::FileChunk {
                    transfer_id,
                    offset,
                    data,
                }) => {
                    if let Err(e) = self.file_transfer.on_file_chunk(transfer_id, offset, &data) {
                        tracing::error!(transfer_id, "file chunk error: {e}");
                    }
                }
                Ok(InboundEvent::FileDone {
                    transfer_id,
                    sha256,
                }) => {
                    if let Err(e) = self.file_transfer.on_file_done(transfer_id, &sha256) {
                        tracing::error!(transfer_id, "file done error: {e}");
                    }
                }
                Ok(InboundEvent::Resume {
                    session_token,
                    last_sequence,
                }) => {
                    if session_token == self.session_token {
                        tracing::info!(last_sequence, "client resume accepted, forcing keyframe");
                        // Send ResumeOk so client knows it can reuse its decoder
                        let _ = self.sender.send_msg(&Message::ResumeOk);
                        // Force keyframe by resetting the timer
                        self.last_keyframe_time = Instant::now() - Duration::from_secs(3600);
                        // Reset sequence to sync with client
                        self.sequence = last_sequence;
                    } else {
                        tracing::warn!("client sent Resume with invalid token");
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
        Ok(())
    }

    /// Poll local clipboard and sync changes to the client (~250ms interval).
    pub fn poll_clipboard(&mut self) -> Result<()> {
        if self.clipboard_poll.elapsed() >= Duration::from_millis(250) {
            self.clipboard_poll = Instant::now();
            if let Some(ref mut ab) = self.arboard {
                if let Ok(text) = ab.get_text() {
                    if let Some(changed) = self.clipboard.check_local_change(&text) {
                        self.sender.send_msg(&Message::ClipboardSync(changed))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Send a video frame, track stats and congestion.
    pub fn send_video_frame(
        &mut self,
        encoded: EncodedFrame,
        congestion: Option<&mut CongestionTracker>,
    ) -> Result<()> {
        if encoded.is_keyframe {
            self.last_keyframe_time = Instant::now();
        }
        self.stats_bytes += encoded.data.len() as u64;
        self.stats_frames += 1;
        self.sequence += 1;

        let send_start = Instant::now();
        self.sender.send_msg(&Message::VideoFrame {
            sequence: self.sequence,
            frame: Box::new(encoded),
        })?;
        if let Some(cg) = congestion {
            cg.on_frame_sent(send_start.elapsed());
        }
        Ok(())
    }

    /// Send a full-frame lossless (zstd) tile update for quality refinement.
    pub fn send_lossless_update(
        &mut self,
        zstd_encoder: &mut ZstdEncoder,
        frame: &Frame,
    ) -> Result<()> {
        let mut fresh = TileDiffer::new();
        let all_tiles = fresh.diff(frame);
        let encoded = zstd_encoder.encode_tiles(&all_tiles)?;
        self.sequence += 1;
        self.sender.send_msg(&Message::TileUpdate {
            sequence: self.sequence,
            tiles: Box::new(encoded),
        })?;
        Ok(())
    }

    /// Send keepalive ping (~1s interval). Returns `Err` if connection lost.
    pub fn keepalive_tick(&mut self) -> Result<()> {
        if self.keepalive_time.elapsed() >= Duration::from_secs(1) {
            self.keepalive_time = Instant::now();
            self.ping_sent_at = Some(Instant::now());
            if self.sender.send_msg(&Message::Ping).is_err() {
                anyhow::bail!("connection lost (keepalive failed)");
            }
        }
        Ok(())
    }

    /// Record the time spent encoding a frame.
    pub fn record_encode_time(&mut self, dur: Duration) {
        self.stats_encode_us += dur.as_micros() as u64;
    }

    /// Check adaptive bitrate and update encoder if needed.
    /// Call this in the stats period (every ~5s).
    pub fn adapt_bitrate(
        &self,
        abr: &mut AdaptiveBitrate,
        congestion: &CongestionTracker,
        encoder: &mut dyn FrameEncoder,
    ) {
        if let Some(new_kbps) = abr.evaluate(self.rtt_us, congestion.skip_ratio) {
            match encoder.set_bitrate_kbps(new_kbps) {
                Ok(()) => abr.apply(new_kbps),
                Err(e) => tracing::warn!("adaptive bitrate change failed: {e}"),
            }
        }
    }

    /// Log stats every 5s and send Stats message to client.
    pub fn log_stats(&mut self, label: &str) {
        if self.stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = self.stats_time.elapsed().as_secs_f64();
            let fps = self.stats_frames as f64 / elapsed;
            let bw_bps = (self.stats_bytes as f64 / elapsed) as u64;
            let avg_encode_us = if self.stats_frames > 0 {
                self.stats_encode_us / self.stats_frames
            } else {
                0
            };
            let rtt_str = match self.rtt_us {
                Some(us) => format!("{:.1}ms", us as f64 / 1000.0),
                None => "n/a".to_string(),
            };
            tracing::info!(
                fps = format_args!("{:.1}", fps),
                bw = format_args!("{:.1} KB/s", bw_bps as f64 / 1024.0),
                rtt = %rtt_str,
                encode_ms = format_args!("{:.1}", avg_encode_us as f64 / 1000.0),
                "{label}"
            );

            // Send Stats to client for overlay display
            let _ = self.sender.send_msg(&Message::Stats {
                rtt_us: self.rtt_us.unwrap_or(0),
                fps: fps as f32,
                bandwidth_bps: bw_bps,
                encode_us: avg_encode_us,
            });

            self.stats_time = Instant::now();
            self.stats_frames = 0;
            self.stats_bytes = 0;
            self.stats_encode_us = 0;
        }
    }

    /// Sleep until the frame interval, pumping input and audio between sleeps.
    pub fn frame_pace(&mut self, loop_start: Instant) -> Result<()> {
        while loop_start.elapsed() < self.frame_interval {
            self.pump_events()?;
            self.drain_audio()?; // keep audio flowing during frame pacing
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// Whether it's time to force a periodic keyframe (every 2s).
    pub fn needs_keyframe(&self) -> bool {
        self.last_keyframe_time.elapsed() >= Duration::from_secs(2)
    }

    /// Consume the `had_input` flag, returning its previous value.
    pub fn take_had_input(&mut self) -> bool {
        let v = self.had_input;
        self.had_input = false;
        v
    }

    /// Set a dedicated audio sender (separate WebSocket).
    pub fn set_audio_sender(&mut self, sender: Box<dyn MessageSender>) {
        tracing::info!("audio channel upgraded to dedicated WebSocket");
        self.audio_sender = Some(sender);
    }

    /// Send any pending audio frames to the client.
    /// Returns the number of audio frames sent.
    pub fn drain_audio(&mut self) -> Result<u32> {
        #[cfg(feature = "audio")]
        {
            let mut count = 0u32;
            if let Some(ref rx) = self.audio_rx {
                while let Ok(chunk) = rx.try_recv() {
                    let msg = Message::AudioFrame {
                        codec: AudioCodec::Opus,
                        sample_rate: chunk.sample_rate,
                        channels: chunk.channels,
                        data: chunk.data,
                    };
                    // Use dedicated audio sender if available (independent WebSocket),
                    // fall back to main sender (shared with video).
                    if let Some(ref mut audio_tx) = self.audio_sender {
                        if audio_tx.send_msg(&msg).is_err() {
                            // Audio WS disconnected — fall back to main
                            self.audio_sender = None;
                            self.sender.send_msg(&msg)?;
                        }
                    } else {
                        self.sender.send_msg(&msg)?;
                    }
                    count += 1;
                }
            }
            Ok(count)
        }
        #[cfg(not(feature = "audio"))]
        {
            Ok(0)
        }
    }

    /// Drain pending file transfer messages and send them to the client.
    pub fn drain_file_transfers(&mut self) -> Result<()> {
        let msgs = self.file_transfer.drain_send_events();
        for msg in msgs {
            self.sender.send_msg(&msg)?;
        }
        Ok(())
    }

    /// Initiate sending a file to the connected client.
    /// The file is read and sent in a background thread.
    pub fn send_file(&mut self, path: &std::path::Path) -> Result<u64> {
        self.file_transfer.initiate_send(path)
    }
}

// ── Session configuration ───────────────────────────────────────────────────

/// Configuration for starting a session, avoids long parameter lists.
/// Result of a session run — carries the session token for reconnect.
pub struct SessionResult {
    pub session_token: Vec<u8>,
    /// The error that ended the session (disconnect, cancel, IO error).
    pub error: anyhow::Error,
}

pub struct SessionConfig<'a> {
    pub sender: Box<dyn MessageSender>,
    pub receiver: Box<dyn MessageReceiver>,
    pub frame_interval: Duration,
    pub quality_delay: Duration,
    pub cancel: Arc<AtomicBool>,
    pub send_file: Option<&'a std::path::Path>,
    pub video_codec: phantom_core::encode::VideoCodec,
    /// If true, this is a resumed session — skip Hello, send keyframe immediately.
    pub is_resume: bool,
    /// Optional input forwarder for service mode (IPC to agent).
    pub input_forwarder: Option<Box<dyn InputForwarder>>,
    /// Receiver for audio-only WebSocket connections (independent from video).
    pub audio_ws_rx: Option<mpsc::Receiver<crate::transport_ws::WsSender>>,
}

// ── Session entry points (one per pipeline) ─────────────────────────────────

/// CPU session: scrap capture + openh264/nvenc encode + tile differ + lossless.
#[allow(clippy::too_many_arguments)]
pub fn run_session_cpu(
    capture: &mut dyn phantom_core::capture::FrameCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    cfg: SessionConfig<'_>,
) -> SessionResult {
    let result = run_session_cpu_inner(capture, video_encoder, differ, cfg);
    match result {
        Ok(token) => SessionResult {
            session_token: token,
            error: anyhow::anyhow!("session ended cleanly"),
        },
        Err(e) => SessionResult {
            session_token: vec![],
            error: e,
        },
    }
}

fn run_session_cpu_inner(
    capture: &mut dyn phantom_core::capture::FrameCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    cfg: SessionConfig<'_>,
) -> Result<Vec<u8>> {
    differ.reset();
    let _ = capture.reset();

    let (width, height) = capture.resolution();
    let mut runner = SessionRunner::new(
        cfg.sender,
        cfg.receiver,
        width,
        height,
        cfg.frame_interval,
        cfg.cancel,
        cfg.video_codec,
        cfg.is_resume,
    )?;
    runner.input_forwarder = cfg.input_forwarder;
    runner.audio_ws_rx = cfg.audio_ws_rx;

    // Send file if requested via --send-file
    if let Some(path) = cfg.send_file {
        if let Err(e) = runner.send_file(path) {
            tracing::error!("failed to initiate file send: {e}");
        }
    }

    // Nudge the screen for DXGI (Windows) — harmless on Linux.
    if let Some(ref mut inj) = runner.injector {
        let _ = inj.inject(&InputEvent::MouseMove { x: 0, y: 0 });
        let _ = inj.inject(&InputEvent::MouseMove { x: 1, y: 1 });
    }

    let mut zstd_encoder = ZstdEncoder::new(3);
    let mut quality = QualityState::new(cfg.quality_delay);
    let mut congestion = CongestionTracker::new(cfg.frame_interval);
    let mut abr = AdaptiveBitrate::new(video_encoder.bitrate_kbps().max(5000));
    let mut last_frame: Option<Frame> = None;
    let mut sent_first_frame = false;
    let mut sent_first_frame_encoded = false;

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;

        // Drain audio FIRST — tiny packets (~100 bytes), must not be blocked by video
        runner.drain_audio()?;

        // Capture
        let frame = match capture.capture()? {
            Some(f) => f,
            None => {
                if quality.should_send_lossless() {
                    if let Some(ref f) = last_frame {
                        runner.send_lossless_update(&mut zstd_encoder, f)?;
                        quality.mark_lossless_sent();
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        };

        let had_input = runner.take_had_input();
        let changed = !sent_first_frame || had_input || differ.has_changes(&frame);

        if changed {
            sent_first_frame = true;
            let dirty_tiles = differ.diff(&frame);
            quality.on_motion();

            if congestion.should_skip_frame() {
                last_frame = Some(frame);
                continue;
            }

            if !dirty_tiles.is_empty() {
                if runner.needs_keyframe() || !sent_first_frame_encoded {
                    video_encoder.force_keyframe();
                }
                let enc_start = Instant::now();
                let encoded = video_encoder.encode_frame(&frame)?;
                runner.record_encode_time(enc_start.elapsed());
                if encoded.is_keyframe && !sent_first_frame_encoded {
                    tracing::info!(size = encoded.data.len(), "first keyframe sent");
                }
                sent_first_frame_encoded = true;
                runner.send_video_frame(encoded, Some(&mut congestion))?;
            }

            last_frame = Some(frame);
        }

        runner.drain_file_transfers()?;
        runner.adapt_bitrate(&mut abr, &congestion, video_encoder);
        runner.log_stats("stats");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

/// IPC forwarding session: receives pre-encoded H.264 from agent, forwards to client.
/// Used by Windows Service mode — agent does DXGI capture + NVENC encode in user session,
/// service just relays the encoded bytes. No re-encoding, no raw frame transfer.
#[cfg(target_os = "windows")]
pub fn run_session_ipc(
    ipc: &crate::ipc_pipe::IpcServer,
    cfg: SessionConfig<'_>,
    initial_width: u32,
    initial_height: u32,
) -> SessionResult {
    let result = run_session_ipc_inner(ipc, cfg, initial_width, initial_height);
    match result {
        Ok(token) => SessionResult {
            session_token: token,
            error: anyhow::anyhow!("session ended cleanly"),
        },
        Err(e) => SessionResult {
            session_token: vec![],
            error: e,
        },
    }
}

#[cfg(target_os = "windows")]
fn run_session_ipc_inner(
    ipc: &crate::ipc_pipe::IpcServer,
    cfg: SessionConfig<'_>,
    initial_width: u32,
    initial_height: u32,
) -> Result<Vec<u8>> {
    let mut runner = SessionRunner::new(
        cfg.sender,
        cfg.receiver,
        initial_width,
        initial_height,
        cfg.frame_interval,
        cfg.cancel,
        cfg.video_codec,
        cfg.is_resume,
    )?;
    runner.input_forwarder = cfg.input_forwarder;
    runner.audio_ws_rx = cfg.audio_ws_rx;

    // Request keyframe from agent for the new client
    let _ = ipc.request_keyframe();

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;
        runner.drain_audio()?;

        // Forward ALL queued encoded frames from agent (H.264 must be sequential)
        for ipc_frame in ipc.recv_encoded_frames() {
            runner.send_video_frame(ipc_frame.encoded, None)?;
        }

        runner.drain_file_transfers()?;
        runner.log_stats("stats-ipc");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

/// Linux GPU zero-copy session: NVFBC grab → NVENC encode → send.
#[cfg(target_os = "linux")]
pub fn run_session_gpu(
    capture: &mut phantom_gpu::nvfbc::NvfbcCapture,
    encoder: &mut phantom_gpu::nvenc::NvencEncoder,
    cfg: SessionConfig<'_>,
) -> SessionResult {
    let result = run_session_gpu_inner(capture, encoder, cfg);
    match result {
        Ok(token) => SessionResult {
            session_token: token,
            error: anyhow::anyhow!("session ended cleanly"),
        },
        Err(e) => SessionResult {
            session_token: vec![],
            error: e,
        },
    }
}

#[cfg(target_os = "linux")]
fn run_session_gpu_inner(
    capture: &mut phantom_gpu::nvfbc::NvfbcCapture,
    encoder: &mut phantom_gpu::nvenc::NvencEncoder,
    cfg: SessionConfig<'_>,
) -> Result<Vec<u8>> {
    use phantom_core::encode::FrameEncoder;

    encoder.force_keyframe();
    let (width, height) = encoder.dimensions();
    let mut runner = SessionRunner::new(
        cfg.sender,
        cfg.receiver,
        width,
        height,
        cfg.frame_interval,
        cfg.cancel,
        cfg.video_codec,
        cfg.is_resume,
    )?;
    runner.input_forwarder = cfg.input_forwarder;
    runner.audio_ws_rx = cfg.audio_ws_rx;

    if let Some(path) = cfg.send_file {
        if let Err(e) = runner.send_file(path) {
            tracing::error!("failed to initiate file send: {e}");
        }
    }

    let mut no_frame_count: u32 = 0;
    let mut abr = AdaptiveBitrate::new(encoder.bitrate_kbps().max(5000));
    // GPU path doesn't use CongestionTracker (no skip), but ABR needs one for the API
    let congestion = CongestionTracker::new(cfg.frame_interval);

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;
        runner.drain_audio()?; // audio first, before capture/encode

        // After input, give the screen a moment to update then grab.
        if runner.had_input {
            std::thread::sleep(Duration::from_millis(2));
            runner.had_input = false;
            no_frame_count = 0;
        }

        capture.bind_context()?;
        let gpu_frame = capture.grab_cuda();
        let _ = capture.release_context();

        match gpu_frame {
            Ok(Some(f)) => {
                no_frame_count = 0;
                let pitch = f.infer_nv12_pitch().unwrap_or(f.width);
                let enc_start = Instant::now();
                let encoded = encoder.encode_device_nv12(f.device_ptr, pitch)?;
                runner.record_encode_time(enc_start.elapsed());
                runner.send_video_frame(encoded, None)?;
            }
            Ok(None) => {
                no_frame_count += 1;
                if no_frame_count > 5 {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(e) => {
                tracing::warn!("GPU grab error: {e}");
            }
        }

        runner.drain_file_transfers()?;
        runner.adapt_bitrate(&mut abr, &congestion, encoder);
        runner.log_stats("GPU stats");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

/// Windows DXGI→NVENC zero-copy session loop.
#[cfg(target_os = "windows")]
pub fn run_session_dxgi(
    pipeline: &mut phantom_gpu::dxgi_nvenc::DxgiNvencPipeline,
    cfg: SessionConfig<'_>,
) -> SessionResult {
    let result = run_session_dxgi_inner(pipeline, cfg);
    match result {
        Ok(token) => SessionResult {
            session_token: token,
            error: anyhow::anyhow!("session ended cleanly"),
        },
        Err(e) => SessionResult {
            session_token: vec![],
            error: e,
        },
    }
}

#[cfg(target_os = "windows")]
fn run_session_dxgi_inner(
    pipeline: &mut phantom_gpu::dxgi_nvenc::DxgiNvencPipeline,
    cfg: SessionConfig<'_>,
) -> Result<Vec<u8>> {
    pipeline.force_keyframe();
    let (width, height) = (pipeline.width, pipeline.height);
    let mut runner = SessionRunner::new(
        cfg.sender,
        cfg.receiver,
        width,
        height,
        cfg.frame_interval,
        cfg.cancel,
        cfg.video_codec,
        cfg.is_resume,
    )?;
    runner.input_forwarder = cfg.input_forwarder;
    runner.audio_ws_rx = cfg.audio_ws_rx;

    if let Some(path) = cfg.send_file {
        if let Err(e) = runner.send_file(path) {
            tracing::error!("failed to initiate file send: {e}");
        }
    }

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;
        runner.drain_audio()?; // audio first, before capture/encode

        // Periodic keyframe
        if runner.needs_keyframe() {
            pipeline.force_keyframe();
        }

        // Capture + encode (zero-copy, all GPU)
        let enc_start = Instant::now();
        match pipeline.capture_and_encode()? {
            Some(encoded) => {
                runner.record_encode_time(enc_start.elapsed());
                runner.send_video_frame(encoded, None)?;
            }
            None => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        }

        runner.drain_file_transfers()?;
        runner.log_stats("stats (DXGI→NVENC)");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Hide the remote OS cursor so mouse movement doesn't cause dirty tiles.
fn hide_remote_cursor() {
    #[cfg(target_os = "linux")]
    {
        if std::process::Command::new("unclutter")
            .args(["-idle", "0", "-root"])
            .spawn()
            .is_ok()
        {
            tracing::info!("remote cursor hidden (unclutter)");
            return;
        }

        if std::process::Command::new("xdotool")
            .args(["search", "--name", ".*"])
            .output()
            .is_ok()
        {
            tracing::debug!("xdotool available but cursor hiding limited");
        }

        tracing::debug!("could not hide remote cursor (install 'unclutter' for best results)");
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::debug!("remote cursor hiding not implemented for this OS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_state_initial_motion() {
        let qs = QualityState::new(Duration::from_secs(2));
        // Just created — should not send lossless yet (delay not elapsed)
        assert!(!qs.should_send_lossless());
    }

    #[test]
    fn quality_state_sends_lossless_after_idle() {
        let mut qs = QualityState::new(Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(15));
        assert!(qs.should_send_lossless());
        qs.mark_lossless_sent();
        assert!(!qs.should_send_lossless());
    }

    #[test]
    fn quality_state_resets_on_motion() {
        let mut qs = QualityState::new(Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(15));
        assert!(qs.should_send_lossless());
        qs.on_motion();
        assert!(!qs.should_send_lossless());
    }

    #[test]
    fn congestion_no_skip_initially() {
        let mut ct = CongestionTracker::new(Duration::from_millis(33));
        // First several frames should not be skipped
        for _ in 0..10 {
            assert!(!ct.should_skip_frame());
        }
    }

    #[test]
    fn congestion_skips_after_slow_frames() {
        let mut ct = CongestionTracker::new(Duration::from_millis(33));
        // Simulate 4 slow frames (>2x frame interval)
        for _ in 0..4 {
            ct.on_frame_sent(Duration::from_millis(80));
        }
        // After 4 slow frames, skip_ratio should be 2
        assert_eq!(ct.skip_ratio, 2);
        // Should skip every other frame
        let skipped: usize = (0..10).filter(|_| ct.should_skip_frame()).count();
        assert!(skipped > 0, "should skip some frames under congestion");
    }

    #[test]
    fn congestion_recovers_after_fast_frames() {
        let mut ct = CongestionTracker::new(Duration::from_millis(33));
        // First enter congestion
        for _ in 0..4 {
            ct.on_frame_sent(Duration::from_millis(80));
        }
        assert!(ct.skip_ratio > 1);
        // Then recover
        for _ in 0..10 {
            ct.on_frame_sent(Duration::from_millis(5));
        }
        assert_eq!(ct.skip_ratio, 1);
    }

    #[test]
    fn congestion_caps_at_4() {
        let mut ct = CongestionTracker::new(Duration::from_millis(33));
        // Spam many slow frames
        for _ in 0..100 {
            ct.on_frame_sent(Duration::from_millis(200));
        }
        assert_eq!(ct.skip_ratio, 4, "skip_ratio should cap at 4");
    }

    #[test]
    fn abr_no_change_initially() {
        let mut abr = AdaptiveBitrate::new(5000);
        // Within the 5s hysteresis window — should not change
        assert!(abr.evaluate(Some(200_000), 1).is_none());
    }

    #[test]
    fn abr_decreases_on_congestion_rtt() {
        let mut abr = AdaptiveBitrate::new(5000);
        // Force past the 5s hysteresis
        abr.last_change = Instant::now() - Duration::from_secs(10);
        // Establish a low baseline RTT first
        abr.baseline_rtt_ms = Some(30.0);
        // RTT = 150ms with baseline 30ms → congested (150 > 30*1.5=45 && 150 > 30+30=60)
        let new = abr.evaluate(Some(150_000), 1);
        assert!(new.is_some(), "should decrease on congestion");
        let new_kbps = new.unwrap();
        assert!(new_kbps < 5000, "should decrease: got {new_kbps}");
        assert_eq!(new_kbps, 3500); // 5000 * 0.7 = 3500
    }

    #[test]
    fn abr_decreases_on_congestion() {
        let mut abr = AdaptiveBitrate::new(5000);
        abr.last_change = Instant::now() - Duration::from_secs(10);
        // Good RTT but congested
        let new = abr.evaluate(Some(30_000), 2);
        assert!(new.is_some());
        assert!(new.unwrap() < 5000);
    }

    #[test]
    fn abr_respects_minimum() {
        let mut abr = AdaptiveBitrate::new(1500); // min_kbps defaults to 1500
        abr.last_change = Instant::now() - Duration::from_secs(10);
        abr.baseline_rtt_ms = Some(30.0);
        // Already at minimum — 1500 * 0.7 = 1050, clamped to 1500 = no change
        let new = abr.evaluate(Some(200_000), 1);
        assert!(new.is_none(), "should not go below minimum");
    }

    #[test]
    fn abr_no_increase_without_stability() {
        let mut abr = AdaptiveBitrate::new(5000);
        abr.last_change = Instant::now() - Duration::from_secs(10);
        // Good RTT but just started — stable_since is None, need 10s stability
        let new = abr.evaluate(Some(20_000), 1);
        assert!(new.is_none());
    }

    #[test]
    fn abr_increases_after_stability() {
        let mut abr = AdaptiveBitrate::new(5000);
        abr.last_change = Instant::now() - Duration::from_secs(10);
        abr.stable_since = Some(Instant::now() - Duration::from_secs(15));
        // Good RTT + stable for >10s → should increase
        let new = abr.evaluate(Some(20_000), 1);
        assert!(new.is_some());
        let new_kbps = new.unwrap();
        assert!(new_kbps > 5000, "should increase: got {new_kbps}");
        assert_eq!(new_kbps, 6000); // 5000 * 1.2 = 6000
    }
}
