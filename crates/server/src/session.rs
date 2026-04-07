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
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ── Inbound events from the network receive thread ──────────────────────────

pub enum InboundEvent {
    Input(InputEvent),
    Clipboard(String),
    PasteText(String),
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
        .spawn(move || {
            loop {
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
                    Ok(_) => {}
                    Err(_) => {
                        let _ = tx.send(InboundEvent::Disconnected);
                        break;
                    }
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
                tracing::info!(skip_ratio = self.skip_ratio, "reducing frame rate (congestion)");
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
}

impl SessionRunner {
    /// Create a new session runner, spawn the receive thread, send Hello, and
    /// hide the remote cursor.
    pub fn new(
        sender: Box<dyn MessageSender>,
        receiver: Box<dyn MessageReceiver>,
        width: u32,
        height: u32,
        frame_interval: Duration,
    ) -> Result<Self> {
        let event_rx = spawn_receive_thread(receiver);
        let injector = InputInjector::new().ok();
        let clipboard = ClipboardTracker::new();
        let arboard = arboard::Clipboard::new().ok();

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
        };

        runner.sender.send_msg(&Message::Hello {
            width,
            height,
            format: phantom_core::frame::PixelFormat::Bgra8,
        })?;
        tracing::info!(width, height, "session started");

        hide_remote_cursor();

        Ok(runner)
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
            tracing::info!(
                fps = format_args!("{:.1}", self.stats_frames as f64 / elapsed),
                bw = format_args!("{:.1} KB/s", self.stats_bytes as f64 / elapsed / 1024.0),
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
}

// ── Session entry points (one per pipeline) ─────────────────────────────────

/// CPU session: scrap capture + openh264/nvenc encode + tile differ + lossless.
pub fn run_session_cpu(
    capture: &mut dyn phantom_core::capture::FrameCapture,
    video_encoder: &mut dyn FrameEncoder,
    differ: &mut TileDiffer,
    sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
    quality_delay: Duration,
) -> Result<()> {
    video_encoder.force_keyframe();
    differ.reset();
    let _ = capture.reset();

    let (width, height) = capture.resolution();
    let mut runner = SessionRunner::new(sender, receiver, width, height, frame_interval)?;

    // Nudge the screen for DXGI (Windows) — harmless on Linux.
    if let Some(ref mut inj) = runner.injector {
        let _ = inj.inject(&InputEvent::MouseMove { x: 0, y: 0 });
        let _ = inj.inject(&InputEvent::MouseMove { x: 1, y: 1 });
    }

    let mut zstd_encoder = ZstdEncoder::new(3);
    let mut quality = QualityState::new(quality_delay);
    let mut congestion = CongestionTracker::new(frame_interval);
    let mut last_frame: Option<Frame> = None;
    let mut sent_first_frame = false;
    let mut sent_first_frame_encoded = false;

    loop {
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
    sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
) -> Result<()> {
    use phantom_core::encode::FrameEncoder;

    encoder.force_keyframe();
    let (width, height) = encoder.dimensions();
    let mut runner = SessionRunner::new(sender, receiver, width, height, frame_interval)?;

    let mut no_frame_count: u32 = 0;

    loop {
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

        runner.log_stats("GPU stats");
        runner.keepalive_tick()?;
        runner.frame_pace(loop_start)?;
    }
}

/// Windows DXGI→NVENC zero-copy session loop.
#[cfg(target_os = "windows")]
pub fn run_session_dxgi(
    pipeline: &mut phantom_gpu::dxgi_nvenc::DxgiNvencPipeline,
    sender: Box<dyn MessageSender>,
    receiver: Box<dyn MessageReceiver>,
    frame_interval: Duration,
) -> Result<()> {
    pipeline.force_keyframe();
    let (width, height) = (pipeline.width, pipeline.height);
    let mut runner = SessionRunner::new(sender, receiver, width, height, frame_interval)?;

    loop {
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

        tracing::debug!(
            "could not hide remote cursor (install 'unclutter' for best results)"
        );
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::debug!("remote cursor hiding not implemented for this OS");
    }
}
