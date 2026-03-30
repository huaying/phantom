//! Encoder comparison benchmark: OpenH264 (CPU) vs NVENC (GPU).
//!
//! Tests multiple resolutions and bitrates, prints a comparison table.
//! NVENC is gracefully skipped if no NVIDIA GPU is available.
//!
//! Run: cargo run --release -p phantom-bench

use anyhow::Result;
use phantom_core::encode::FrameEncoder;
use phantom_core::frame::{Frame, PixelFormat};
use phantom_gpu::cuda::CudaLib;
use phantom_gpu::nvenc::NvencEncoder;
use std::sync::Arc;
use std::time::Instant;

const ROUNDS: usize = 50;
const WARMUP: usize = 3;

const RESOLUTIONS: &[(u32, u32, &str)] = &[
    (1280, 720, "720p"),
    (1920, 1080, "1080p"),
    (2560, 1440, "1440p"),
    (3840, 2160, "4K"),
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

/// BGRA to NV12 conversion (CPU) — for measuring per-stage NVENC costs.
fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, nv12: &mut [u8]) {
    let (y_plane, uv_plane) = nv12.split_at_mut(w * h);
    for row in 0..h {
        for col in 0..w {
            let i = (row * w + col) * 4;
            let (b, g, r) = (bgra[i] as i32, bgra[i + 1] as i32, bgra[i + 2] as i32);
            y_plane[row * w + col] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
            if row % 2 == 0 && col % 2 == 0 {
                let ui = (row / 2) * w + col;
                uv_plane[ui] =
                    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                uv_plane[ui + 1] =
                    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        }
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

    // --- NVENC per-stage breakdown ---
    if let Some(ref cuda) = cuda {
        println!("\n=== NVENC per-stage breakdown ===\n");
        println!(
            "{:<10} {:>14} {:>14} {:>14} {:>14}",
            "Res", "Color Conv", "Memcpy H→D", "Encode", "Total"
        );
        println!("{}", "-".repeat(72));

        for &(w, h, label) in RESOLUTIONS {
            let frame = make_frame(w, h);
            let nv12_size = (w as usize) * (h as usize) * 3 / 2;
            let mut nv12 = vec![0u8; nv12_size];

            // Stage 1: BGRA → NV12 color conversion
            let color = bench(ROUNDS, || {
                bgra_to_nv12(&frame.data, w as usize, h as usize, &mut nv12);
                0
            });

            // Stages 2+3 require a working NVENC encoder
            if let Ok(mut enc) = NvencEncoder::new(Arc::clone(cuda), 0, w, h, 30, 5000) {
                // Full encode to measure total
                let total = bench(ROUNDS, || {
                    enc.encode_frame(&frame).map(|ef| ef.data.len()).unwrap_or(0)
                });

                // Encode time ~ total - color_convert (memcpy is small but included)
                let encode_est = (total.avg_ms - color.avg_ms).max(0.0);

                println!(
                    "{label:<10} {:>12.2}ms {:>12}  {:>12.2}ms {:>12.2}ms",
                    color.avg_ms, "(included)", encode_est, total.avg_ms
                );
            } else {
                println!("{label:<10} {:>12.2}ms {:>12}  {:>12}  {:>12}", color.avg_ms, "-", "-", "-");
            }
        }
        println!();
    }

    // --- Speedup summary ---
    if cuda.is_some() {
        println!("=== Speedup summary (1080p, 5000kbps) ===\n");
        let (w, h) = (1920, 1080);
        let frame = make_frame(w, h);
        let bitrate = 5000;

        let oh264_time = OpenH264Bench::new(w, h, 30, bitrate).ok().map(|mut enc| {
            bench(ROUNDS, || enc.encode(&frame).unwrap_or(0)).avg_ms
        });
        let nvenc_time = cuda.as_ref().and_then(|c| {
            NvencEncoder::new(Arc::clone(c), 0, w, h, 30, bitrate).ok()
        }).map(|mut enc| {
            bench(ROUNDS, || enc.encode_frame(&frame).map(|ef| ef.data.len()).unwrap_or(0)).avg_ms
        });

        match (oh264_time, nvenc_time) {
            (Some(oh), Some(nv)) => {
                println!("  OpenH264:  {oh:.2}ms");
                println!("  NVENC:     {nv:.2}ms");
                println!("  Speedup:   {:.1}x", oh / nv);
            }
            _ => println!("  Could not compare (one encoder unavailable)."),
        }
    }

    println!("\ndone.");
}
