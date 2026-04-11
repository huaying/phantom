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
    skip_ratio: u32,
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

// ── SessionRunner: shared session plumbing ──────────────────────────────────

pub struct SessionRunner {
    pub sender: Box<dyn MessageSender>,
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
}

impl SessionRunner {
    /// Create a new session runner, spawn the receive thread, send Hello, and
    /// hide the remote cursor. Optionally starts audio capture.
    pub fn new(
        sender: Box<dyn MessageSender>,
        receiver: Box<dyn MessageReceiver>,
        width: u32,
        height: u32,
        frame_interval: Duration,
        cancel: Arc<AtomicBool>,
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
            last_keyframe_time: Instant::now(),
            ping_sent_at: None,
            rtt_us: None,
            cancel,
            #[cfg(feature = "audio")]
            audio_rx,
            #[cfg(feature = "audio")]
            _audio_capture: audio_capture,
            file_transfer: crate::file_transfer::ServerFileTransfer::new(),
        };

        runner.sender.send_msg(&Message::Hello {
            width,
            height,
            format: phantom_core::frame::PixelFormat::Bgra8,
            protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
            audio: has_audio,
        })?;
        tracing::info!(width, height, audio = has_audio, "session started");

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
        loop {
            match self.event_rx.try_recv() {
                Ok(InboundEvent::Input(event)) => {
                    if let Some(ref mut inj) = self.injector {
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

    /// Log stats every 5s. Returns the label to use (caller can override).
    pub fn log_stats(&mut self, label: &str) {
        if self.stats_time.elapsed() >= Duration::from_secs(5) {
            let elapsed = self.stats_time.elapsed().as_secs_f64();
            let rtt_str = match self.rtt_us {
                Some(us) => format!("{:.1}ms", us as f64 / 1000.0),
                None => "n/a".to_string(),
            };
            tracing::info!(
                fps = format_args!("{:.1}", self.stats_frames as f64 / elapsed),
                bw = format_args!("{:.1} KB/s", self.stats_bytes as f64 / elapsed / 1024.0),
                rtt = %rtt_str,
                "{label}"
            );
            self.stats_time = Instant::now();
            self.stats_frames = 0;
            self.stats_bytes = 0;
        }
    }

    /// Sleep until the frame interval, pumping input events between sleeps.
    pub fn frame_pace(&mut self, loop_start: Instant) -> Result<()> {
        while loop_start.elapsed() < self.frame_interval {
            self.pump_events()?;
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

    /// Send any pending audio frames to the client.
    /// Returns the number of audio frames sent.
    pub fn drain_audio(&mut self) -> Result<u32> {
        #[cfg(feature = "audio")]
        {
            let mut count = 0u32;
            if let Some(ref rx) = self.audio_rx {
                while let Ok(chunk) = rx.try_recv() {
                    self.sender.send_msg(&Message::AudioFrame {
                        codec: AudioCodec::Opus,
                        sample_rate: chunk.sample_rate,
                        channels: chunk.channels,
                        data: chunk.data,
                    })?;
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
pub struct SessionConfig<'a> {
    pub sender: Box<dyn MessageSender>,
    pub receiver: Box<dyn MessageReceiver>,
    pub frame_interval: Duration,
    pub quality_delay: Duration,
    pub cancel: Arc<AtomicBool>,
    pub send_file: Option<&'a std::path::Path>,
}

// ── Session entry points (one per pipeline) ─────────────────────────────────

/// CPU session: scrap capture + openh264/nvenc encode + tile differ + lossless.
#[allow(clippy::too_many_arguments)]
pub fn run_session_cpu(
    capture: &mut dyn phantom_core::capture::FrameCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    cfg: SessionConfig<'_>,
) -> Result<()> {
    video_encoder.force_keyframe();
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
    )?;

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
    let mut last_frame: Option<Frame> = None;
    let mut sent_first_frame = false;
    let mut sent_first_frame_encoded = false;

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;

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
                let encoded = video_encoder.encode_frame(&frame)?;
                if encoded.is_keyframe && !sent_first_frame_encoded {
                    tracing::info!(size = encoded.data.len(), "first keyframe sent");
                }
                sent_first_frame_encoded = true;
                runner.send_video_frame(encoded, Some(&mut congestion))?;
            }

            last_frame = Some(frame);
        }

        runner.drain_audio()?;
        runner.drain_file_transfers()?;
        runner.log_stats("stats");
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
) -> Result<()> {
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
    )?;

    if let Some(path) = cfg.send_file {
        if let Err(e) = runner.send_file(path) {
            tracing::error!("failed to initiate file send: {e}");
        }
    }

    let mut no_frame_count: u32 = 0;

    loop {
        runner.check_cancelled()?;
        let loop_start = Instant::now();

        runner.pump_events()?;
        runner.poll_clipboard()?;

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
                let encoded = encoder.encode_device_nv12(f.device_ptr, pitch)?;
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

        runner.drain_audio()?;
        runner.drain_file_transfers()?;
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
) -> Result<()> {
    pipeline.force_keyframe();
    let (width, height) = (pipeline.width, pipeline.height);
    let mut runner = SessionRunner::new(
        cfg.sender,
        cfg.receiver,
        width,
        height,
        cfg.frame_interval,
        cfg.cancel,
    )?;

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

        // Periodic keyframe
        if runner.needs_keyframe() {
            pipeline.force_keyframe();
        }

        // Capture + encode (zero-copy, all GPU)
        match pipeline.capture_and_encode()? {
            Some(encoded) => {
                runner.send_video_frame(encoded, None)?;
            }
            None => {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
        }

        runner.drain_audio()?;
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
}
