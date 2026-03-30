//! GPU-focused tests: NVENC init + encode, NVFBC grab, zero-copy pipeline.
//! Run: cargo run --example nvenc_bench -p phantom-gpu (requires NVIDIA GPU)

use phantom_core::capture::FrameCapture;
use phantom_core::encode::FrameEncoder;
use phantom_core::frame::{Frame, PixelFormat};
use phantom_gpu::cuda::CudaLib;
use phantom_gpu::nvenc::NvencEncoder;
use std::sync::Arc;
use std::time::Instant;

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

fn main() {
    let cuda = match CudaLib::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            println!("No NVIDIA GPU available: {e}");
            println!("Run this on a machine with NVIDIA drivers.");
            return;
        }
    };

    // --- Test 1: NVENC init + encode a few frames ---
    println!("=== Test 1: NVENC init + encode ===\n");
    let (w, h) = (1920, 1080);
    let frame = make_frame(w, h);
    match NvencEncoder::new(Arc::clone(&cuda), 0, w, h, 30, 5000) {
        Ok(mut enc) => {
            for i in 0..5 {
                let t = Instant::now();
                let result = enc.encode_frame(&frame);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                match result {
                    Ok(ef) => println!(
                        "  frame {i}: {:.2}ms, {} bytes, keyframe={}",
                        ms, ef.data.len(), ef.is_keyframe
                    ),
                    Err(e) => println!("  frame {i}: encode error: {e}"),
                }
            }
            println!("  NVENC encode: OK\n");
        }
        Err(e) => println!("  NVENC init failed: {e}\n"),
    }

    // --- Test 2: NVFBC grab ---
    println!("=== Test 2: NVFBC capture ===\n");
    let dev = cuda.device_get(0).unwrap();
    let ctx = cuda.ctx_create(dev).unwrap();
    match phantom_gpu::nvfbc::NvfbcCapture::new(
        Arc::clone(&cuda), ctx, phantom_gpu::sys::NVFBC_BUFFER_FORMAT_NV12,
    ) {
        Ok(mut cap) => {
            let (cw, ch) = cap.resolution();
            println!("  NVFBC opened: {cw}x{ch}");

            // Grab a few frames
            for i in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(16));
                match cap.grab_cuda() {
                    Ok(Some(f)) => println!(
                        "  grab {i}: {}x{} ptr=0x{:x} size={}",
                        f.width, f.height, f.device_ptr, f.byte_size
                    ),
                    Ok(None) => println!("  grab {i}: no new frame"),
                    Err(e) => println!("  grab {i}: error: {e}"),
                }
            }
            println!("  NVFBC grab: OK\n");

            // --- Test 3: NVFBC → NVENC zero-copy pipeline ---
            println!("=== Test 3: NVFBC → NVENC zero-copy ===\n");
            match NvencEncoder::new(Arc::clone(&cuda), 0, cw, ch, 30, 5000) {
                Ok(mut enc) => {
                    for i in 0..3 {
                        std::thread::sleep(std::time::Duration::from_millis(16));
                        if let Ok(Some(f)) = cap.grab_cuda() {
                            let t = Instant::now();
                            match enc.encode_device_nv12(f.device_ptr, f.width) {
                                Ok(ef) => println!(
                                    "  zero-copy {i}: {:.2}ms, {} bytes, keyframe={}",
                                    t.elapsed().as_secs_f64() * 1000.0,
                                    ef.data.len(), ef.is_keyframe
                                ),
                                Err(e) => println!("  zero-copy {i}: encode error: {e}"),
                            }
                        }
                    }
                    println!("  Zero-copy pipeline: OK\n");
                }
                Err(e) => println!("  NVENC init for zero-copy failed: {e}\n"),
            }
        }
        Err(e) => println!("  NVFBC not available: {e}\n"),
    }

    println!("done.");
}
