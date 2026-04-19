//! Pipeline trait — single abstraction over "capture + encode" so the session
//! loop can dispatch to CPU / NVFBC-NVENC / DXGI-NVENC through one code path.
//!
//! See task #23 for the motivating refactor. The three original session
//! runners in `session.rs` (run_session_cpu / run_session_gpu /
//! run_session_dxgi) duplicated ~600 lines of congestion, adaptive-bitrate,
//! keepalive, input, and stats plumbing. Pipeline pushes the per-backend
//! differences (tile diff vs zero-copy GPU, different congestion handling,
//! different keyframe triggers) behind one method.

use crate::session::CongestionTracker;
use anyhow::Result;
use phantom_core::capture::FrameCapture;
use phantom_core::encode::{EncodedFrame, FrameEncoder};
use phantom_core::tile::TileDiffer;
use std::time::{Duration, Instant};

/// Information the session loop hands to the pipeline each tick.
pub struct TickCtx {
    /// True iff the session saw an input event since the last tick.
    /// Pipelines that gate encoding on "did the screen change" (the CPU tile
    /// differ) use this to force a re-encode after a keystroke — the screen
    /// often changes shortly after input, but the capture might race.
    pub had_input: bool,
    /// True iff a periodic keyframe is due. Pipelines should honor this on
    /// the next encode.
    pub needs_keyframe: bool,
}

/// What the pipeline produced this tick.
pub struct TickResult {
    pub encoded: EncodedFrame,
    /// Wall-clock time the pipeline spent in its encode call. Used for stats.
    pub encode_duration: Duration,
}

/// A full capture+encode pipeline. One tick yields at most one encoded frame.
///
/// Not `Send` — pipelines are constructed and driven on a single thread (the
/// session thread). Capture backends hold platform-specific handles (X11
/// connection, D3D11 device, CUDA context) that aren't thread-portable, so
/// leaving Send out keeps the trait honest.
pub trait Pipeline {
    /// Attempt to produce the next encoded frame. Returns `Ok(None)` if the
    /// capture had nothing fresh, the differ saw no change, or congestion
    /// control told us to skip — the session loop will sleep briefly and
    /// come back.
    fn tick(&mut self, ctx: TickCtx) -> Result<Option<TickResult>>;

    /// Current capture resolution.
    fn dimensions(&self) -> (u32, u32);

    /// Current target bitrate (kbps). The session loop reads this at
    /// startup to seed `AdaptiveBitrate`.
    fn bitrate_kbps(&self) -> u32;

    /// Adjust the target bitrate. Default: unsupported (most encoders can't
    /// change mid-stream without a rebuild). Implementations that support
    /// ABR should override.
    fn set_bitrate_kbps(&mut self, _kbps: u32) -> Result<()> {
        anyhow::bail!("pipeline does not support runtime bitrate change")
    }

    /// If this pipeline tracks its own congestion state, expose it so the
    /// session can (a) pass it to `send_video_frame` for per-send timing and
    /// (b) read `skip_ratio` for the adaptive-bitrate decision. GPU pipelines
    /// typically return `None` — they can't usefully skip a frame that's
    /// already been GPU-encoded, and the send path is fast.
    fn congestion_mut(&mut self) -> Option<&mut CongestionTracker> {
        None
    }

    /// Label used for the stats log line (e.g. "stats", "stats (DXGI→NVENC)").
    fn log_label(&self) -> &'static str {
        "stats"
    }

    /// Called once after Hello is sent and before the loop starts. Default:
    /// no-op. GPU pipelines use this to force the first frame to be a
    /// keyframe (CPU pipelines do that inside `tick` based on `needs_keyframe`).
    fn prepare(&mut self) -> Result<()> {
        Ok(())
    }
}

// ── CpuPipeline ─────────────────────────────────────────────────────────────

/// Traditional split capture → encode. TileDiffer gates encodes when the
/// screen hasn't moved. CongestionTracker skips frames under network pressure.
pub struct CpuPipeline<'a> {
    capture: &'a mut dyn FrameCapture,
    encoder: &'a mut dyn FrameEncoder,
    differ: &'a mut TileDiffer,
    congestion: CongestionTracker,
    sent_first_frame: bool,
    sent_first_frame_encoded: bool,
}

impl<'a> CpuPipeline<'a> {
    pub fn new(
        capture: &'a mut dyn FrameCapture,
        encoder: &'a mut dyn FrameEncoder,
        differ: &'a mut TileDiffer,
        frame_interval: Duration,
    ) -> Result<Self> {
        differ.reset();
        let _ = capture.reset();
        Ok(Self {
            capture,
            encoder,
            differ,
            congestion: CongestionTracker::new(frame_interval),
            sent_first_frame: false,
            sent_first_frame_encoded: false,
        })
    }
}

impl<'a> Pipeline for CpuPipeline<'a> {
    fn tick(&mut self, ctx: TickCtx) -> Result<Option<TickResult>> {
        let frame = match self.capture.capture()? {
            Some(f) => f,
            None => return Ok(None),
        };

        let changed =
            !self.sent_first_frame || ctx.had_input || self.differ.has_changes(&frame);
        if !changed {
            return Ok(None);
        }
        self.sent_first_frame = true;

        let dirty_tiles = self.differ.diff(&frame);

        if self.congestion.should_skip_frame() {
            return Ok(None);
        }

        if dirty_tiles.is_empty() {
            return Ok(None);
        }

        if ctx.needs_keyframe || !self.sent_first_frame_encoded {
            self.encoder.force_keyframe();
        }

        let enc_start = Instant::now();
        let encoded = self.encoder.encode_frame(&frame)?;
        let encode_duration = enc_start.elapsed();

        if encoded.is_keyframe && !self.sent_first_frame_encoded {
            tracing::info!(size = encoded.data.len(), "first keyframe sent");
        }
        self.sent_first_frame_encoded = true;

        Ok(Some(TickResult {
            encoded,
            encode_duration,
        }))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.capture.resolution()
    }

    fn bitrate_kbps(&self) -> u32 {
        self.encoder.bitrate_kbps()
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        self.encoder.set_bitrate_kbps(kbps)
    }

    fn congestion_mut(&mut self) -> Option<&mut CongestionTracker> {
        Some(&mut self.congestion)
    }
}
