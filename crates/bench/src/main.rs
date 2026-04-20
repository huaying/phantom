//! Encoder + fused-pipeline benchmark.
//!
//! Covers the paths phantom actually ships as of 0.4.10:
//!
//! - **Component** (FrameEncoder trait): OpenH264 CPU, NVENC GPU
//!   (H.264 + AV1 on Ada Lovelace+).
//! - **Fused GPU pipelines** (capture + encode, zero-copy):
//!   NVFBC→NVENC on Linux, DXGI→NVENC on Windows. Both codecs.
//!
//! Run:
//!   cargo run --release -p phantom-bench                # default mix
//!   DISPLAY=:0 cargo run --release -p phantom-bench     # include NVFBC
//!
//! NVENC is skipped if no NVIDIA GPU. AV1 is skipped if the GPU doesn't
//! advertise AV1 encode support (probe). NVFBC/DXGI sections no-op on
//! the wrong OS.

use anyhow::Result;
#[cfg(target_os = "linux")]
use phantom_core::capture::FrameCapture;
use phantom_core::encode::{FrameEncoder, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_gpu::cuda::CudaLib;
use phantom_gpu::nvenc::NvencEncoder;
use std::sync::Arc;
use std::time::Instant;

const ROUNDS: usize = 30;
const WARMUP: usize = 3;

const RESOLUTIONS: &[(u32, u32, &str)] = &[
    (1280, 720, "720p"),
    (1920, 1080, "1080p"),
    (2560, 1440, "1440p"),
];

const BITRATES_KBPS: &[u32] = &[3000, 5000, 10000];

/// Generate a frame whose content varies with `seed`. Fixed-pattern
/// gradients compress unrealistically well; adding per-frame noise
/// forces the encoder to actually produce non-trivial P-frames.
fn make_frame(w: u32, h: u32, seed: u32) -> Frame {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            data[i] = ((x + seed * 7) % 256) as u8; // B
            data[i + 1] = ((y + seed * 13) % 256) as u8; // G
            data[i + 2] = ((x.wrapping_mul(y).wrapping_add(seed * 31)) % 256) as u8; // R
            data[i + 3] = 255;
        }
    }
    Frame {
        width: w,
        height: h,
        format: PixelFormat::Bgra8,
        data,
        timestamp: Instant::now(),
    }
}

#[derive(Clone)]
struct BenchResult {
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    avg_bytes: usize,
}

impl BenchResult {
    fn summary(&self) -> String {
        format!(
            "{:>6} B/f {:>6.2}ms avg / {:>6.2}ms p50 / {:>6.2}ms p95",
            self.avg_bytes, self.avg_ms, self.p50_ms, self.p95_ms,
        )
    }
}

/// Run `f` for `ROUNDS` rounds after `WARMUP`; return stats. `f` returns
/// encoded size in bytes (0 on skip).
fn bench(mut f: impl FnMut(u32) -> usize) -> BenchResult {
    for i in 0..WARMUP {
        f(i as u32);
    }
    let mut times = Vec::with_capacity(ROUNDS);
    let mut total_bytes = 0usize;
    for i in 0..ROUNDS {
        let t = Instant::now();
        total_bytes += f((i + WARMUP) as u32);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg_ms = times.iter().sum::<f64>() / times.len() as f64;
    let p50_ms = times[times.len() / 2];
    let p95_ms = times[(times.len() as f64 * 0.95) as usize];
    let avg_bytes = total_bytes / ROUNDS;
    BenchResult {
        avg_ms,
        p50_ms,
        p95_ms,
        avg_bytes,
    }
}

// ── OpenH264 CPU wrapper (H.264 only) ───────────────────────────────────────

struct OpenH264Bench {
    encoder: openh264::encoder::Encoder,
    width: usize,
    height: usize,
}

struct BgraSource<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl openh264::formats::RGBSource for BgraSource<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let idx = (y * self.width + x) * 4;
        let b = self.data[idx] as f32;
        let g = self.data[idx + 1] as f32;
        let r = self.data[idx + 2] as f32;
        (r, g, b)
    }
}

impl OpenH264Bench {
    fn new(width: u32, height: u32, bitrate_kbps: u32) -> Result<Self> {
        let config = openh264::encoder::EncoderConfig::new()
            .max_frame_rate(30.0)
            .set_bitrate_bps(bitrate_kbps * 1000)
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .enable_skip_frame(true);
        let api = openh264::OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config)?;
        Ok(Self {
            encoder,
            width: width as usize,
            height: height as usize,
        })
    }

    fn encode(&mut self, frame: &Frame) -> Result<usize> {
        let src = BgraSource {
            data: &frame.data,
            width: self.width,
            height: self.height,
        };
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(src);
        let bs = self.encoder.encode(&yuv)?;
        Ok(bs.to_vec().len())
    }
}

// ── Component bench (encoder only) ──────────────────────────────────────────

fn component_bench(cuda: Option<&Arc<CudaLib>>, av1_supported: bool) {
    println!("=== Component bench (encoder only, {ROUNDS} rounds + {WARMUP} warmup) ===\n");
    println!(
        "{:<8} {:>8} {:<10} Result",
        "Res", "Bitrate", "Encoder"
    );
    println!("{}", "-".repeat(80));

    for &(w, h, label) in RESOLUTIONS {
        for &bitrate in BITRATES_KBPS {
            // OpenH264 (CPU, H.264 only)
            match OpenH264Bench::new(w, h, bitrate) {
                Ok(mut enc) => {
                    let r = bench(|seed| {
                        let f = make_frame(w, h, seed);
                        enc.encode(&f).unwrap_or(0)
                    });
                    println!(
                        "{label:<8} {:>5}k {:<10} {}",
                        bitrate,
                        "openh264",
                        r.summary()
                    );
                }
                Err(e) => {
                    println!("{label:<8} {:>5}k openh264   init failed: {e}", bitrate);
                }
            }

            // NVENC H.264
            if let Some(cuda) = cuda {
                if let Ok(mut enc) =
                    NvencEncoder::new(Arc::clone(cuda), 0, w, h, 30, bitrate, VideoCodec::H264)
                {
                    let r = bench(|seed| {
                        let f = make_frame(w, h, seed);
                        enc.encode_frame(&f).map(|ef| ef.data.len()).unwrap_or(0)
                    });
                    println!(
                        "{label:<8} {:>5}k {:<10} {}",
                        bitrate,
                        "nvenc/h264",
                        r.summary()
                    );
                }
            }

            // NVENC AV1 (only where GPU supports it)
            if let (Some(cuda), true) = (cuda, av1_supported) {
                if let Ok(mut enc) =
                    NvencEncoder::new(Arc::clone(cuda), 0, w, h, 30, bitrate, VideoCodec::Av1)
                {
                    let r = bench(|seed| {
                        let f = make_frame(w, h, seed);
                        enc.encode_frame(&f).map(|ef| ef.data.len()).unwrap_or(0)
                    });
                    println!(
                        "{label:<8} {:>5}k {:<10} {}",
                        bitrate,
                        "nvenc/av1",
                        r.summary()
                    );
                }
            }
        }
        println!();
    }
}

// ── NVFBC→NVENC fused (Linux only) ──────────────────────────────────────────
//
// NVFBC + xdotool: NVFBC's default NOWAIT mode only yields a frame when
// the screen actually changed. On a static test rig that means zero
// frames. FORCE_REFRESH would solve this but blocks on driver 550 (see
// docs/pitfalls.md). The workaround is a background mouse-jiggle process
// that forces screen updates. The Drop guard below kills it on exit
// (including panics / Ctrl+C) so we don't leak xdotool processes that
// would drift a real phantom session's cursor.

#[cfg(target_os = "linux")]
struct XdotoolJiggle(Option<std::process::Child>);

#[cfg(target_os = "linux")]
impl XdotoolJiggle {
    fn start() -> Self {
        let child = std::process::Command::new("bash")
            .args([
                "-c",
                "while true; do xdotool mousemove \
                 $((RANDOM%1920)) $((RANDOM%1080)) 2>/dev/null; sleep 0.01; done",
            ])
            .env("DISPLAY", ":0")
            .spawn()
            .ok();
        Self(child)
    }
}

#[cfg(target_os = "linux")]
impl Drop for XdotoolJiggle {
    fn drop(&mut self) {
        // Kill our own child first, then pkill any strays. Belt + suspenders
        // because Ctrl+C can leave the parent intact but kill this process
        // before the explicit .kill() lands.
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::process::Command::new("pkill")
            .args(["-f", "xdotool mousemove"])
            .status();
    }
}

#[cfg(target_os = "linux")]
fn nvfbc_nvenc_bench(cuda: &Arc<CudaLib>, codec: VideoCodec, codec_label: &str) {
    let dev = match cuda.device_get(0) {
        Ok(d) => d,
        Err(_) => return,
    };
    let primary_ctx = match cuda.primary_ctx_retain(dev) {
        Ok(c) => c,
        Err(e) => {
            println!("  primary_ctx_retain failed: {e}");
            return;
        }
    };
    let _pop_guard = PrimaryCtxGuard {
        cuda: Arc::clone(cuda),
        dev,
        ctx: primary_ctx,
    };
    unsafe { cuda.ctx_push(primary_ctx) }.ok();

    let mut cap = match phantom_gpu::nvfbc::NvfbcCapture::with_options(
        Arc::clone(cuda),
        primary_ctx,
        phantom_gpu::sys::NVFBC_BUFFER_FORMAT_NV12,
        true,
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("  NVFBC not available ({e})");
            return;
        }
    };
    let (sw, sh) = cap.resolution();
    println!("  screen: {sw}x{sh}  codec: {codec_label}");

    let _jiggle = XdotoolJiggle::start();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Wait for first frame.
    let first = loop {
        match cap.grab_cuda() {
            Ok(Some(f)) => break f,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                println!("  grab failed: {e}");
                return;
            }
        }
    };
    let (fw, fh) = (first.width, first.height);
    println!("  first frame: {fw}x{fh}");
    cap.release_context().ok();

    let mut enc = match unsafe {
        NvencEncoder::with_context(
            Arc::clone(cuda),
            primary_ctx,
            false,
            fw,
            fh,
            30,
            5000,
            codec,
        )
    } {
        Ok(e) => e,
        Err(e) => {
            println!("  NVENC init failed: {e}");
            return;
        }
    };

    // Warmup
    for _ in 0..WARMUP {
        std::thread::sleep(std::time::Duration::from_millis(16));
        cap.bind_context().ok();
        if let Ok(Some(f)) = cap.grab_cuda() {
            cap.release_context().ok();
            let p = f.infer_nv12_pitch().unwrap_or(f.width);
            let _ = enc.encode_device_nv12(f.device_ptr, p);
        } else {
            cap.release_context().ok();
        }
    }

    // Measure
    let mut cap_times = Vec::new();
    let mut enc_times = Vec::new();
    let mut total_times = Vec::new();
    let mut encoded = 0usize;
    for _ in 0..ROUNDS {
        std::thread::sleep(std::time::Duration::from_millis(16));
        cap.bind_context().ok();
        let t0 = Instant::now();
        let grabbed = cap.grab_cuda();
        let cap_ms = t0.elapsed().as_secs_f64() * 1000.0;
        cap.release_context().ok();

        if let Ok(Some(f)) = grabbed {
            let p = f.infer_nv12_pitch().unwrap_or(f.width);
            let t1 = Instant::now();
            if let Ok(ef) = enc.encode_device_nv12(f.device_ptr, p) {
                let enc_ms = t1.elapsed().as_secs_f64() * 1000.0;
                cap_times.push(cap_ms);
                enc_times.push(enc_ms);
                total_times.push(cap_ms + enc_ms);
                encoded += ef.data.len();
            }
        }
    }
    if total_times.is_empty() {
        println!("  no frames captured");
        return;
    }
    let n = total_times.len();
    cap_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    enc_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    total_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let p50 = |v: &[f64]| v[v.len() / 2];
    println!("  frames: {n}/{ROUNDS}");
    println!(
        "  capture: avg {:.2}ms  p50 {:.2}ms",
        avg(&cap_times),
        p50(&cap_times)
    );
    println!(
        "  encode:  avg {:.2}ms  p50 {:.2}ms",
        avg(&enc_times),
        p50(&enc_times)
    );
    println!(
        "  total:   avg {:.2}ms  p50 {:.2}ms",
        avg(&total_times),
        p50(&total_times)
    );
    println!("  avg size: {} B/f", encoded / n);
}

#[cfg(target_os = "linux")]
struct PrimaryCtxGuard {
    cuda: Arc<CudaLib>,
    dev: i32,
    #[allow(dead_code)]
    ctx: phantom_gpu::sys::CUcontext,
}

#[cfg(target_os = "linux")]
impl Drop for PrimaryCtxGuard {
    fn drop(&mut self) {
        self.cuda.ctx_pop().ok();
        self.cuda.primary_ctx_release(self.dev);
    }
}

// ── DXGI→NVENC fused (Windows only) ─────────────────────────────────────────

#[cfg(target_os = "windows")]
fn dxgi_nvenc_bench() {
    // DxgiNvencPipeline is H.264-only today (Baseline profile, see
    // crates/gpu/src/dxgi_nvenc.rs). AV1 would need a parallel pipeline
    // variant — not wired up.
    println!("  codec: h264 (DxgiNvencPipeline is H.264-only)");
    let mut pipeline = match phantom_gpu::dxgi_nvenc::DxgiNvencPipeline::new(30, 5000) {
        Ok(p) => p,
        Err(e) => {
            println!("  DXGI→NVENC init failed: {e}");
            return;
        }
    };
    println!("  first frame: {}x{}", pipeline.width, pipeline.height);

    // Warmup
    for _ in 0..WARMUP {
        std::thread::sleep(std::time::Duration::from_millis(16));
        let _ = pipeline.capture_and_encode();
    }

    let mut times = Vec::new();
    let mut encoded = 0usize;
    for _ in 0..ROUNDS {
        std::thread::sleep(std::time::Duration::from_millis(16));
        let t = Instant::now();
        if let Ok(Some(ef)) = pipeline.capture_and_encode() {
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            encoded += ef.data.len();
        }
    }
    if times.is_empty() {
        println!("  no frames captured");
        return;
    }
    let n = times.len();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = times.iter().sum::<f64>() / n as f64;
    let p50 = times[n / 2];
    let p95 = times[(n as f64 * 0.95) as usize];
    println!("  frames:   {n}/{ROUNDS}");
    println!("  total:    avg {avg:.2}ms  p50 {p50:.2}ms  p95 {p95:.2}ms");
    println!("  avg size: {} B/f", encoded / n);
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    println!("Phantom Encoder Benchmark (v{})", env!("CARGO_PKG_VERSION"));
    println!("{}", "=".repeat(60));
    println!();

    let cuda = match CudaLib::load() {
        Ok(c) => {
            println!("NVIDIA GPU detected — NVENC + NVFBC enabled");
            Some(Arc::new(c))
        }
        Err(e) => {
            println!("No NVIDIA GPU ({e}) — NVENC / NVFBC / DXGI sections skipped");
            None
        }
    };

    // Quick AV1 probe
    let av1_supported = cuda
        .as_ref()
        .map(|cuda| {
            NvencEncoder::new(Arc::clone(cuda), 0, 320, 240, 30, 1000, VideoCodec::Av1).is_ok()
        })
        .unwrap_or(false);
    if cuda.is_some() {
        println!(
            "AV1 encode: {}",
            if av1_supported {
                "available"
            } else {
                "not supported by this GPU"
            }
        );
    }
    println!();

    component_bench(cuda.as_ref(), av1_supported);

    #[cfg(target_os = "linux")]
    if let Some(ref cuda) = cuda {
        println!("=== NVFBC → NVENC fused (Linux zero-copy) ===\n");
        nvfbc_nvenc_bench(cuda, VideoCodec::H264, "h264");
        println!();
        if av1_supported {
            nvfbc_nvenc_bench(cuda, VideoCodec::Av1, "av1");
            println!();
        }
    }

    #[cfg(target_os = "windows")]
    if cuda.is_some() {
        println!("=== DXGI → NVENC fused (Windows zero-copy) ===\n");
        dxgi_nvenc_bench();
        println!();
    }

    println!("done.");
}
