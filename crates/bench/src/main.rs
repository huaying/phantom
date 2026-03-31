//! Encoder comparison benchmark: OpenH264 (CPU) vs NVENC (GPU).
//!
//! Tests multiple resolutions and bitrates, prints a comparison table.
//! NVENC is gracefully skipped if no NVIDIA GPU is available.
//!
//! Run: cargo run --release -p phantom-bench

use anyhow::Result;
#[cfg(target_os = "linux")]
use phantom_core::capture::FrameCapture;
use phantom_core::encode::FrameEncoder;
use phantom_core::frame::{Frame, PixelFormat};
use phantom_gpu::cuda::CudaLib;
use phantom_gpu::nvenc::NvencEncoder;
use std::sync::Arc;
use std::time::Instant;

const ROUNDS: usize = 20;
const WARMUP: usize = 2;

const RESOLUTIONS: &[(u32, u32, &str)] = &[
    (1280, 720, "720p"),
    (1920, 1080, "1080p"),
    (2560, 1440, "1440p"),
];

const BITRATES_KBPS: &[u32] = &[3000, 5000, 10000];

fn make_frame(w: u32, h: u32) -> Frame {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            data[i] = (x % 256) as u8;
            data[i + 1] = (y % 256) as u8;
            data[i + 2] = 128;
            data[i + 3] = 255;
        }
    }
    Frame { width: w, height: h, format: PixelFormat::Bgra8, data, timestamp: Instant::now() }
}

struct BenchResult {
    avg_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    avg_bytes: usize,
}

fn bench(rounds: usize, mut f: impl FnMut() -> usize) -> BenchResult {
    // warmup
    for _ in 0..WARMUP {
        f();
    }

    let mut times = Vec::with_capacity(rounds);
    let mut total_bytes = 0;
    for _ in 0..rounds {
        let t = Instant::now();
        total_bytes += f();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg_ms = times.iter().sum::<f64>() / times.len() as f64;
    let p50_ms = times[times.len() / 2];
    let p95_ms = times[(times.len() as f64 * 0.95) as usize];
    let avg_bytes = total_bytes / rounds;
    BenchResult { avg_ms, p50_ms, p95_ms, avg_bytes }
}

/// Wrapper for OpenH264 encoder using the openh264 crate directly.
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
    fn new(width: u32, height: u32, _fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let config = openh264::encoder::EncoderConfig::new()
            .max_frame_rate(30.0)
            .set_bitrate_bps(bitrate_kbps * 1000)
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .enable_skip_frame(true);
        let api = openh264::OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config)?;
        Ok(Self { encoder, width: width as usize, height: height as usize })
    }

    fn encode(&mut self, frame: &Frame) -> Result<usize> {
        let src = BgraSource { data: &frame.data, width: self.width, height: self.height };
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(src);
        let bs = self.encoder.encode(&yuv)?;
        Ok(bs.to_vec().len())
    }
}

fn main() {
    println!("Phantom Encoder Benchmark");
    println!("=========================\n");
    println!("Rounds: {ROUNDS}, Warmup: {WARMUP}\n");

    // Try to load CUDA for NVENC
    let cuda = match CudaLib::load() {
        Ok(c) => {
            println!("NVIDIA GPU detected — NVENC enabled\n");
            Some(Arc::new(c))
        }
        Err(e) => {
            println!("No NVIDIA GPU ({e}) — NVENC will be skipped\n");
            None
        }
    };

    // Header
    println!(
        "{:<10} {:>8}  {:>10} {:>8} {:>8} {:>8}  {:>10} {:>8} {:>8} {:>8}",
        "Res", "Bitrate",
        "OpenH264", "avg", "p50", "p95",
        "NVENC", "avg", "p50", "p95",
    );
    println!("{}", "-".repeat(110));

    for &(w, h, label) in RESOLUTIONS {
        let frame = make_frame(w, h);

        for &bitrate in BITRATES_KBPS {
            // --- OpenH264 ---
            let oh264 = match OpenH264Bench::new(w, h, 30, bitrate) {
                Ok(mut enc) => {
                    let r = bench(ROUNDS, || enc.encode(&frame).unwrap_or(0));
                    Some(r)
                }
                Err(e) => {
                    eprintln!("OpenH264 init failed for {label} {bitrate}kbps: {e}");
                    None
                }
            };

            // --- NVENC ---
            let nvenc = cuda.as_ref().and_then(|c| {
                NvencEncoder::new(Arc::clone(c), 0, w, h, 30, bitrate).ok()
            }).map(|mut enc| {
                bench(ROUNDS, || {
                    enc.encode_frame(&frame).map(|ef| ef.data.len()).unwrap_or(0)
                })
            });

            // Print row
            let oh264_str = match &oh264 {
                Some(r) => format!("{:>6} B/f {:>6.2}ms {:>6.2}ms {:>6.2}ms",
                    r.avg_bytes, r.avg_ms, r.p50_ms, r.p95_ms),
                None => format!("{:>38}", "N/A"),
            };
            let nvenc_str = match &nvenc {
                Some(r) => format!("{:>6} B/f {:>6.2}ms {:>6.2}ms {:>6.2}ms",
                    r.avg_bytes, r.avg_ms, r.p50_ms, r.p95_ms),
                None => format!("{:>38}", "N/A"),
            };
            println!("{label:<10} {:>5}k  {oh264_str}  {nvenc_str}", bitrate);
        }
        println!();
    }

    // --- NVFBC → NVENC zero-copy (Linux only) ---
    #[cfg(target_os = "linux")]
    if let Some(ref cuda) = cuda {
        println!("=== NVFBC → NVENC zero-copy ===\n");

        let dev = cuda.device_get(0).unwrap();
        let primary_ctx = match cuda.primary_ctx_retain(dev) {
            Ok(c) => c,
            Err(e) => { println!("  primary_ctx_retain failed: {e}\n"); println!("done."); return; }
        };
        unsafe { cuda.ctx_push(primary_ctx) }.ok();

        match phantom_gpu::nvfbc::NvfbcCapture::with_options(
            Arc::clone(cuda), primary_ctx, phantom_gpu::sys::NVFBC_BUFFER_FORMAT_NV12, true, // with_cursor for bench
        ) {
            Ok(mut cap) => {
                let (sw, sh) = cap.resolution();
                println!("  screen: {sw}x{sh}");

                // Spawn background mouse movement to generate frame updates.
                // Must be killed when done — stale xdotool processes cause phantom mouse drift.
                let _mouse_mover = std::process::Command::new("bash")
                    .args(["-c", "while true; do xdotool mousemove $((RANDOM%1920)) $((RANDOM%1080)) 2>/dev/null; sleep 0.005; done"])
                    .env("DISPLAY", ":0")
                    .spawn()
                    .ok();
                std::thread::sleep(std::time::Duration::from_millis(100));

                // Wait for first frame
                let first = loop {
                    match cap.grab_cuda() {
                        Ok(Some(f)) => break f,
                        Ok(None) => { std::thread::sleep(std::time::Duration::from_millis(10)); }
                        Err(e) => { println!("  grab failed: {e}"); println!("\ndone."); return; }
                    }
                };
                let (fw, fh) = (first.width, first.height);
                let pitch = first.infer_nv12_pitch().unwrap_or(fw);
                println!("  frame: {fw}x{fh}, pitch={pitch}");

                // Init NVENC sharing primary context
                cap.release_context().ok();
                match unsafe {
                    NvencEncoder::with_context(Arc::clone(cuda), primary_ctx, false, fw, fh, 30, 5000)
                } {
                    Ok(mut enc) => {
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
                            println!("  no frames captured (static desktop?)\n");
                        } else {
                            cap_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                            enc_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                            total_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
                            let n = total_times.len();
                            let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
                            let p50 = |v: &[f64]| v[v.len() / 2];

                            println!("  frames captured: {n}/{ROUNDS}");
                            println!("  capture:  avg {:.2}ms  p50 {:.2}ms", avg(&cap_times), p50(&cap_times));
                            println!("  encode:   avg {:.2}ms  p50 {:.2}ms", avg(&enc_times), p50(&enc_times));
                            println!("  total:    avg {:.2}ms  p50 {:.2}ms", avg(&total_times), p50(&total_times));
                            println!("  avg size: {} B/f", encoded / n);
                        }
                    }
                    Err(e) => println!("  NVENC init failed: {e}"),
                }
            }
            Err(e) => println!("  NVFBC not available: {e}"),
        }

        // Kill any xdotool processes spawned by this bench
        let _ = std::process::Command::new("pkill").args(["-f", "xdotool mousemove"]).status();

        cuda.ctx_pop().ok();
        cuda.primary_ctx_release(dev);
        println!();
    }

    println!("done.");
}
