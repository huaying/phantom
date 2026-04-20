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

        let changed = !self.sent_first_frame || ctx.had_input || self.differ.has_changes(&frame);
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

// ── NvfbcNvencPipeline (Linux GPU zero-copy) ────────────────────────────────

/// NVFBC capture → NVENC encode, zero-copy via a CUDA device pointer. The
/// frame never reaches CPU memory.
///
/// Behavior notes preserved from the pre-refactor `run_session_gpu`:
/// - No congestion tracker: we don't skip frames mid-encode; the GPU pipeline
///   is already fast enough that congestion would have to be upstream.
/// - After an input event, briefly sleep 2ms so the screen has time to
///   actually update before we grab (grab_cuda is eager — grabs identical
///   frames back-to-back otherwise).
/// - `no_frame_count` backoff: after 5 consecutive empty grabs, insert a
///   2ms sleep to avoid hammering the GPU with no-op calls.
///
/// Behavior added by this refactor:
/// - Honor `ctx.needs_keyframe` inside the loop. The old code only forced a
///   keyframe once at startup, which meant a client reconnecting mid-stream
///   couldn't recover decode state until the next session restart. The DXGI
///   path already did this; bringing NVFBC in line.
#[cfg(target_os = "linux")]
pub struct NvfbcNvencPipeline<'a> {
    capture: &'a mut phantom_gpu::nvfbc::NvfbcCapture,
    encoder: &'a mut phantom_gpu::nvenc::NvencEncoder,
    no_frame_count: u32,
}

#[cfg(target_os = "linux")]
impl<'a> NvfbcNvencPipeline<'a> {
    pub fn new(
        capture: &'a mut phantom_gpu::nvfbc::NvfbcCapture,
        encoder: &'a mut phantom_gpu::nvenc::NvencEncoder,
    ) -> Self {
        Self {
            capture,
            encoder,
            no_frame_count: 0,
        }
    }
}

#[cfg(target_os = "linux")]
impl<'a> Pipeline for NvfbcNvencPipeline<'a> {
    fn prepare(&mut self) -> Result<()> {
        self.encoder.force_keyframe();
        Ok(())
    }

    fn tick(&mut self, ctx: TickCtx) -> Result<Option<TickResult>> {
        if ctx.had_input {
            std::thread::sleep(Duration::from_millis(2));
            self.no_frame_count = 0;
        }

        if ctx.needs_keyframe {
            self.encoder.force_keyframe();
        }

        self.capture.bind_context()?;
        let gpu_frame = self.capture.grab_cuda();
        let _ = self.capture.release_context();

        match gpu_frame {
            Ok(Some(f)) => {
                self.no_frame_count = 0;
                let pitch = f.infer_nv12_pitch().unwrap_or(f.width);
                let enc_start = Instant::now();
                let encoded = self.encoder.encode_device_nv12(f.device_ptr, pitch)?;
                let encode_duration = enc_start.elapsed();
                Ok(Some(TickResult {
                    encoded,
                    encode_duration,
                }))
            }
            Ok(None) => {
                self.no_frame_count += 1;
                if self.no_frame_count > 5 {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(None)
            }
            Err(e) => {
                tracing::warn!("GPU grab error: {e}");
                Ok(None)
            }
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        self.encoder.dimensions()
    }

    fn bitrate_kbps(&self) -> u32 {
        use phantom_core::encode::FrameEncoder;
        self.encoder.bitrate_kbps()
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        use phantom_core::encode::FrameEncoder;
        self.encoder.set_bitrate_kbps(kbps)
    }

    fn log_label(&self) -> &'static str {
        "GPU stats"
    }
}

// ── DxgiNvencPipeline adapter (Windows GPU zero-copy) ───────────────────────

/// Thin wrapper around `phantom_gpu::dxgi_nvenc::DxgiNvencPipeline` (the
/// fused DXGI capture + NVENC encoder struct that lives in the gpu crate)
/// so it fits behind our Pipeline trait.
///
/// Behavior preserved:
/// - `prepare()` forces the first frame to be a keyframe — matches the
///   old `run_session_dxgi` which called `pipeline.force_keyframe()` just
///   before entering the loop.
/// - Periodic keyframe via `ctx.needs_keyframe` — matches the old code.
/// - No ABR: the fused pipeline doesn't expose a bitrate setter, so
///   `set_bitrate_kbps` falls back to the default (Err). The session loop
///   logs a warning every ~5s and otherwise ignores it; this matches the
///   old `run_session_dxgi` which simply never called `adapt_bitrate`.
#[cfg(target_os = "windows")]
pub struct DxgiNvencPipelineAdapter<'a> {
    inner: &'a mut phantom_gpu::dxgi_nvenc::DxgiNvencPipeline,
    /// Snapshot of initial bitrate — the inner struct doesn't expose a
    /// getter, and AdaptiveBitrate only needs it for the starting value.
    initial_bitrate_kbps: u32,
}

#[cfg(target_os = "windows")]
impl<'a> DxgiNvencPipelineAdapter<'a> {
    pub fn new(
        inner: &'a mut phantom_gpu::dxgi_nvenc::DxgiNvencPipeline,
        initial_bitrate_kbps: u32,
    ) -> Self {
        Self {
            inner,
            initial_bitrate_kbps,
        }
    }
}

#[cfg(target_os = "windows")]
impl<'a> Pipeline for DxgiNvencPipelineAdapter<'a> {
    fn prepare(&mut self) -> Result<()> {
        self.inner.force_keyframe();
        Ok(())
    }

    fn tick(&mut self, ctx: TickCtx) -> Result<Option<TickResult>> {
        if ctx.needs_keyframe {
            self.inner.force_keyframe();
        }
        let enc_start = Instant::now();
        match self.inner.capture_and_encode()? {
            Some(encoded) => Ok(Some(TickResult {
                encoded,
                encode_duration: enc_start.elapsed(),
            })),
            None => Ok(None),
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    fn bitrate_kbps(&self) -> u32 {
        self.initial_bitrate_kbps
    }

    fn log_label(&self) -> &'static str {
        "stats (DXGI→NVENC)"
    }
}
