//! Session loop regression tests.
//!
//! These tests drive `run_session_cpu` with mock capture/encoder/transport so
//! we can assert the control-flow contract of the session loop without real
//! screen capture, real H.264 encoding, or a real network peer. They exist as
//! a safety net for the Pipeline-trait refactor (task #23) — any observable
//! behavior change there should be visible here.
//!
//! Design notes:
//! - We use real clocks (no time mocking). Tests run for a bounded real-time
//!   window (~300ms-1s), then cancel. Matches the rest of the test suite.
//! - MockCapture yields a configurable number of frames, then returns `None`
//!   forever. MockSender collects messages for assertion. MockReceiver sits
//!   on an mpsc channel so tests can inject input events.
//! - Audio capture starts inside SessionRunner::new when the `audio` feature
//!   is on. It's best-effort (fails silently if no PulseAudio), so tests pass
//!   on macOS / headless Linux.
//! - Tests don't assert exact frame counts — the capture/encode loop runs at
//!   whatever rate the machine allows. We assert lower bounds ("at least one
//!   VideoFrame was sent") and qualitative shape ("first message is Hello").

use anyhow::Result;
use phantom_core::capture::FrameCapture;
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::input::{InputEvent, KeyCode};
use phantom_core::protocol::Message;
use phantom_core::tile::TileDiffer;
use phantom_core::transport::{MessageReceiver, MessageSender};
use phantom_server::session::{run_session_cpu, SessionConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Global serial lock for this test file. Rust runs `#[test]` functions on a
/// thread pool by default; several of the real components SessionRunner::new
/// constructs (notably `arboard` on macOS, which talks to NSPasteboard) are
/// not thread-safe and will segfault under concurrent use. Each test
/// acquires this lock at the top so we effectively run serially.
fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Mocks ───────────────────────────────────────────────────────────────────

/// Returns a fresh frame on every call for `total` calls, then `None` forever.
/// Each frame's BGRA pattern depends on `fill_byte` so static-screen tests can
/// hold it constant while motion tests can vary it.
struct MockCapture {
    width: u32,
    height: u32,
    remaining: u32,
    fill: Arc<Mutex<u8>>,
}

impl MockCapture {
    fn new(width: u32, height: u32, frame_budget: u32) -> (Self, Arc<Mutex<u8>>) {
        let fill = Arc::new(Mutex::new(0u8));
        (
            Self {
                width,
                height,
                remaining: frame_budget,
                fill: fill.clone(),
            },
            fill,
        )
    }
}

impl FrameCapture for MockCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let fill = *self.fill.lock().unwrap();
        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Bgra8,
            data: vec![fill; (self.width * self.height * 4) as usize],
            timestamp: Instant::now(),
        }))
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Returns a fake encoded frame on every call. `force_keyframe()` sets a flag
/// the next encode picks up so we can observe keyframe triggering.
struct MockEncoder {
    bitrate_kbps: Arc<Mutex<u32>>,
    force_kf: Arc<Mutex<bool>>,
    encode_count: Arc<Mutex<u32>>,
    keyframe_count: Arc<Mutex<u32>>,
}

impl MockEncoder {
    fn new() -> (
        Self,
        Arc<Mutex<u32>>, // bitrate
        Arc<Mutex<u32>>, // encode_count
        Arc<Mutex<u32>>, // keyframe_count
    ) {
        let bitrate = Arc::new(Mutex::new(5000u32));
        let encode_count = Arc::new(Mutex::new(0u32));
        let keyframe_count = Arc::new(Mutex::new(0u32));
        (
            Self {
                bitrate_kbps: bitrate.clone(),
                force_kf: Arc::new(Mutex::new(false)),
                encode_count: encode_count.clone(),
                keyframe_count: keyframe_count.clone(),
            },
            bitrate,
            encode_count,
            keyframe_count,
        )
    }
}

impl FrameEncoder for MockEncoder {
    fn encode_frame(&mut self, _frame: &Frame) -> Result<EncodedFrame> {
        *self.encode_count.lock().unwrap() += 1;
        let is_keyframe = {
            let mut kf = self.force_kf.lock().unwrap();
            let was = *kf;
            *kf = false;
            was
        };
        if is_keyframe {
            *self.keyframe_count.lock().unwrap() += 1;
        }
        Ok(EncodedFrame {
            codec: VideoCodec::H264,
            data: vec![0xAB; 64],
            is_keyframe,
        })
    }

    fn force_keyframe(&mut self) {
        *self.force_kf.lock().unwrap() = true;
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        *self.bitrate_kbps.lock().unwrap() = kbps;
        Ok(())
    }

    fn bitrate_kbps(&self) -> u32 {
        *self.bitrate_kbps.lock().unwrap()
    }
}

/// Captures every message the session sends, in order, with a lock for the
/// test thread to inspect. Never errors.
struct MockSender {
    out: Arc<Mutex<Vec<Message>>>,
}

impl MockSender {
    fn new() -> (Self, Arc<Mutex<Vec<Message>>>) {
        let out = Arc::new(Mutex::new(Vec::new()));
        (Self { out: out.clone() }, out)
    }
}

impl MessageSender for MockSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        self.out.lock().unwrap().push(clone_message(msg));
        Ok(())
    }
}

/// Feeds Messages in from an mpsc channel. Tests push InputEvents etc.
struct MockReceiver {
    rx: mpsc::Receiver<Message>,
}

impl MockReceiver {
    fn new() -> (Self, mpsc::Sender<Message>) {
        let (tx, rx) = mpsc::channel();
        (Self { rx }, tx)
    }
}

impl MessageReceiver for MockReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        self.rx
            .recv()
            .map_err(|_| anyhow::anyhow!("mock receiver closed"))
    }

    fn recv_msg_within(&mut self, timeout: Duration) -> Result<Option<Message>> {
        match self.rx.recv_timeout(timeout) {
            Ok(m) => Ok(Some(m)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("mock receiver closed"))
            }
        }
    }
}

// Message isn't Clone (EncodedFrame etc. may not be). Hand-roll just what we
// need for assertions: the variants the session loop actually emits.
fn clone_message(msg: &Message) -> Message {
    match msg {
        Message::Hello {
            width,
            height,
            format,
            protocol_version,
            audio,
            video_codec,
            session_token,
        } => Message::Hello {
            width: *width,
            height: *height,
            format: *format,
            protocol_version: *protocol_version,
            audio: *audio,
            video_codec: *video_codec,
            session_token: session_token.clone(),
        },
        Message::VideoFrame { sequence, frame } => Message::VideoFrame {
            sequence: *sequence,
            frame: Box::new(EncodedFrame {
                codec: frame.codec,
                data: frame.data.clone(),
                is_keyframe: frame.is_keyframe,
            }),
        },
        Message::Ping => Message::Ping,
        Message::Pong => Message::Pong,
        Message::ClipboardSync(s) => Message::ClipboardSync(s.clone()),
        Message::Stats {
            rtt_us,
            fps,
            bandwidth_bps,
            encode_us,
        } => Message::Stats {
            rtt_us: *rtt_us,
            fps: *fps,
            bandwidth_bps: *bandwidth_bps,
            encode_us: *encode_us,
        },
        Message::ResumeOk => Message::ResumeOk,
        Message::Disconnect { reason } => Message::Disconnect {
            reason: reason.clone(),
        },
        // Fallback for anything we don't care to inspect — preserve the
        // discriminant shape as a Ping so test counts don't break.
        _ => Message::Ping,
    }
}

// ── Test harness ────────────────────────────────────────────────────────────

struct Harness {
    cancel: Arc<AtomicBool>,
    sent: Arc<Mutex<Vec<Message>>>,
    input_tx: mpsc::Sender<Message>,
    encode_count: Arc<Mutex<u32>>,
    keyframe_count: Arc<Mutex<u32>>,
    bitrate: Arc<Mutex<u32>>,
    join: thread::JoinHandle<()>,
}

impl Harness {
    /// Spawn a CPU session on a fresh thread and return handles. Runs until
    /// `stop()` is called (or capture exhausts and session blocks — in which
    /// case stop() still wakes it via cancel flag).
    fn start(width: u32, height: u32, frame_budget: u32) -> (Self, Arc<Mutex<u8>>) {
        let (mut capture, fill) = MockCapture::new(width, height, frame_budget);
        let (mut encoder, bitrate, encode_count, keyframe_count) = MockEncoder::new();
        let (sender, sent) = MockSender::new();
        let (receiver, input_tx) = MockReceiver::new();

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();

        let join = thread::spawn(move || {
            let mut differ = TileDiffer::new();
            let cfg = SessionConfig {
                sender: Box::new(sender),
                receiver: Box::new(receiver),
                frame_interval: Duration::from_millis(33),
                cancel: cancel_clone,
                send_file: None,
                video_codec: VideoCodec::H264,
                is_resume: false,
                input_forwarder: None,
                audio_ws_rx: None,
                resolution_change_fn: None,
                paste_fn: None,
            };
            let _ = run_session_cpu(&mut capture, &mut encoder, &mut differ, cfg);
        });

        (
            Self {
                cancel,
                sent,
                input_tx,
                encode_count,
                keyframe_count,
                bitrate,
                join,
            },
            fill,
        )
    }

    fn stop(self) {
        self.cancel.store(true, Ordering::Relaxed);
        // Session loop checks cancel once per iteration (~every 1-33ms in CPU
        // path); give it a generous window before we give up.
        let _ = self.join.join();
    }

    fn sent(&self) -> Vec<Message> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(clone_message)
            .collect()
    }

    /// Block until the session has finished its Hello handshake (first message
    /// in the outbox), or until `deadline` elapses. Returns true if ready.
    /// Needed because SessionRunner::new on macOS takes 300-500ms to
    /// initialize audio capture, which would otherwise race short tests.
    fn wait_for_ready(&self, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if !self.sent.lock().unwrap().is_empty() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

fn count_variants<F: Fn(&Message) -> bool>(msgs: &[Message], pred: F) -> usize {
    msgs.iter().filter(|m| pred(m)).count()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn hello_is_first_message() {
    let _lock = test_lock();
    let (h, _fill) = Harness::start(320, 240, 5);
    assert!(
        h.wait_for_ready(Duration::from_secs(3)),
        "session never sent Hello within 3s"
    );
    let msgs = h.sent();
    h.stop();

    match &msgs[0] {
        Message::Hello {
            width,
            height,
            video_codec,
            ..
        } => {
            assert_eq!(*width, 320);
            assert_eq!(*height, 240);
            assert_eq!(*video_codec, VideoCodec::H264);
        }
        _ => panic!("first message should be Hello, got a different variant"),
    }
}

#[test]
fn emits_video_frames_for_changing_capture() {
    let _lock = test_lock();
    let (h, fill) = Harness::start(64, 64, 300);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    // Change the fill byte every 20ms so TileDiffer thinks each frame is dirty.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        {
            let mut f = fill.lock().unwrap();
            *f = f.wrapping_add(1);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let msgs = h.sent();
    let encode_count = *h.encode_count.lock().unwrap();
    h.stop();

    let video_count = count_variants(&msgs, |m| matches!(m, Message::VideoFrame { .. }));
    assert!(
        video_count >= 2,
        "expected ≥2 VideoFrame messages, got {video_count} (encoder called {encode_count} times)",
    );
}

#[test]
fn first_encoded_frame_is_a_keyframe() {
    let _lock = test_lock();
    // Even with a static capture, the session should force a keyframe on the
    // first encode so the client can start decoding.
    let (h, _fill) = Harness::start(64, 64, 10);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    thread::sleep(Duration::from_millis(300));
    let keyframes = *h.keyframe_count.lock().unwrap();
    h.stop();

    assert!(keyframes >= 1, "expected ≥1 keyframe, got {keyframes}");
}

#[test]
fn static_screen_still_emits_one_frame() {
    let _lock = test_lock();
    // Fill never changes → TileDiffer reports no dirty tiles after frame 1.
    // But the session sends at least one VideoFrame (the first one, which is
    // always sent regardless of diff).
    let (h, _fill) = Harness::start(64, 64, 50);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    thread::sleep(Duration::from_millis(300));
    let msgs = h.sent();
    h.stop();

    let video_count = count_variants(&msgs, |m| matches!(m, Message::VideoFrame { .. }));
    assert!(
        video_count >= 1,
        "expected ≥1 VideoFrame even for static screen, got {video_count}"
    );
}

#[test]
fn cancel_ends_session_cleanly() {
    let _lock = test_lock();
    let (h, _fill) = Harness::start(64, 64, 1000);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    let start = Instant::now();
    h.stop();
    // Session should wake and exit within the next loop iteration (~33ms).
    // Give a generous budget; the mock capture does not block so it's fast.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "session took >2s to honour cancel"
    );
}

#[test]
fn input_channel_does_not_block_session() {
    let _lock = test_lock();
    // Sanity check: spamming input events at the session doesn't deadlock or
    // crash it. We aren't asserting per-event side effects here (injector is
    // real, not mocked — the actual inject call is a no-op when there's no
    // display); just that the session keeps running and producing frames.
    let (h, fill) = Harness::start(64, 64, 300);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    for _ in 0..20 {
        let _ = h.input_tx.send(Message::Input(InputEvent::Key {
            key: KeyCode::A,
            pressed: true,
        }));
        {
            let mut f = fill.lock().unwrap();
            *f = f.wrapping_add(1);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let msgs_before = h.sent().len();
    thread::sleep(Duration::from_millis(100));
    let msgs_after = h.sent().len();
    h.stop();

    assert!(
        msgs_after > msgs_before,
        "session stopped producing messages after input barrage \
         ({msgs_before} → {msgs_after})"
    );
}

#[test]
fn bitrate_remains_sensible_under_idle() {
    let _lock = test_lock();
    // No stress → adaptive bitrate should stay at its starting value (5000 kbps
    // per MockEncoder::new). We just check it didn't blow up to 0 or overflow.
    let (h, _fill) = Harness::start(64, 64, 30);
    assert!(h.wait_for_ready(Duration::from_secs(3)), "session not ready");
    thread::sleep(Duration::from_millis(300));
    let final_kbps = *h.bitrate.lock().unwrap();
    h.stop();

    assert!(
        (100..=100_000).contains(&final_kbps),
        "bitrate drifted to unreasonable value: {final_kbps} kbps"
    );
}
