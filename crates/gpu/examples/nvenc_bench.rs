//! GPU-focused tests: NVENC init + encode, NVFBC grab, zero-copy pipeline.
//! Run: DISPLAY=:0 cargo run --release --example nvenc_bench -p phantom-gpu

use phantom_gpu::cuda::CudaLib;
#[cfg(target_os = "linux")]
use phantom_gpu::nvenc::NvencEncoder;
use phantom_gpu::sys::CUcontext;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Instant;

fn fmt_ctx(ctx: CUcontext) -> String {
    format!("0x{:x}", ctx as usize)
}

fn main() {
    let cuda = match CudaLib::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            println!("No NVIDIA GPU available: {e}");
            return;
        }
    };

    // --- NVFBC capture + NVENC zero-copy ---
    println!("=== NVFBC → NVENC zero-copy ===\n");
    let dev = cuda.device_get(0).unwrap();
    let primary_ctx = cuda.primary_ctx_retain(dev).unwrap();
    println!("  retained primary CUDA context: {}", fmt_ctx(primary_ctx));
    unsafe { cuda.ctx_push(primary_ctx) }.unwrap();
    println!("  pushed primary CUDA context on current thread");

    #[cfg(target_os = "linux")]
    {
        match phantom_gpu::nvfbc::NvfbcCapture::new(
            Arc::clone(&cuda),
            primary_ctx,
            phantom_gpu::sys::NVFBC_BUFFER_FORMAT_NV12,
        ) {
            Ok(mut cap) => {
                let (sw, sh) = cap.resolution();
                let rv = cap.runtime_version();
                println!(
                    "  screen: {sw}x{sh}, NVFBC v{}.{}",
                    (rv >> 8) & 0xff,
                    rv & 0xff
                );

                let current_after_nvfbc = cuda.ctx_get_current().unwrap_or(std::ptr::null_mut());
                println!(
                    "  CUDA current after NVFBC init: {} (same as primary: {})",
                    fmt_ctx(current_after_nvfbc),
                    current_after_nvfbc == primary_ctx
                );
                let shared_ctx = if current_after_nvfbc.is_null() {
                    primary_ctx
                } else {
                    current_after_nvfbc
                };
                println!("  context used for NVENC: {}", fmt_ctx(shared_ctx));

                // Grab one frame to get real dimensions (context bound from init).
                let first = loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    match cap.grab_cuda() {
                        Ok(Some(f)) => break f,
                        Ok(None) => continue,
                        Err(e) => {
                            println!("  grab failed: {e}");
                            return;
                        }
                    }
                };
                // Release NVFBC context so NVENC can init
                cap.release_context().expect("release failed");
                let (fw, fh) = (first.width, first.height);
                let first_pitch = first.infer_nv12_pitch().unwrap_or(fw);
                println!("  frame: {fw}x{fh}, inferred_pitch={first_pitch}\n");

                match unsafe {
                    NvencEncoder::with_context(
                        Arc::clone(&cuda),
                        shared_ctx,
                        false,
                        fw,
                        fh,
                        30,
                        5000,
                        phantom_core::encode::VideoCodec::H264,
                    )
                } {
                    Ok(mut enc) => {
                        for i in 0..10 {
                            // Strict handoff state machine:
                            // 1) NVFBC bind -> grab
                            // 2) NVFBC release
                            // 3) NVENC encode
                            let cap_t = Instant::now();
                            if let Err(e) = cap.bind_context() {
                                println!("  frame {i}: bind_context error: {e}");
                                break;
                            }
                            let f = match cap.grab_cuda() {
                                Ok(Some(f)) => f,
                                Ok(None) => {
                                    cap.release_context().ok();
                                    std::thread::sleep(std::time::Duration::from_millis(5));
                                    continue;
                                }
                                Err(e) => {
                                    cap.release_context().ok();
                                    println!("  frame {i}: grab error: {e}");
                                    break;
                                }
                            };
                            cap.release_context().ok();

                            let cap_ms = cap_t.elapsed().as_secs_f64() * 1000.0;
                            let pitch = f.infer_nv12_pitch().unwrap_or(f.width);
                            if pitch != f.width {
                                println!(
                                    "  frame {i}: using pitch={pitch} (width={}, byte_size={})",
                                    f.width, f.byte_size
                                );
                            }

                            let enc_t = Instant::now();
                            match enc.encode_device_nv12(f.device_ptr, pitch) {
                                Ok(ef) => {
                                    let enc_ms = enc_t.elapsed().as_secs_f64() * 1000.0;
                                    println!(
                                        "  frame {i}: capture {cap_ms:.2}ms + encode {enc_ms:.2}ms = {:.2}ms, {} bytes, kf={}, pitch={}",
                                        cap_ms + enc_ms, ef.data.len(), ef.is_keyframe, pitch
                                    );
                                }
                                Err(e) => {
                                    println!("  frame {i}: encode error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => println!("  NVENC init failed: {e}"),
                }
            }
            Err(e) => println!("  NVFBC not available: {e}"),
        }
    }

    let _ = cuda.ctx_pop();
    cuda.primary_ctx_release(dev);

    println!("\ndone.");
}
