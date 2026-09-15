//! GPU-only regression: synthetic NVENC access units -> NVDEC -> RGB pixels.
//! Run with `cargo run -p phantom-gpu --release --features nvdec --example nvdec_smoke`.
//! Does not access a desktop, clipboard, input device or audio endpoint.

use anyhow::{ensure, Result};
use phantom_core::encode::{FrameDecoder, FrameEncoder, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_gpu::{cuda::CudaLib, nvdec::NvdecDecoder, nvenc::NvencEncoder};
use std::{sync::Arc, time::Instant};

fn main() -> Result<()> {
    let cuda = Arc::new(CudaLib::load()?);
    let mut encoder =
        NvencEncoder::new(Arc::clone(&cuda), 0, 640, 360, 30, 5000, VideoCodec::H264)?;
    let caller_context = cuda.ctx_get_current()?;
    let mut decoder = NvdecDecoder::new(Arc::clone(&cuda), 0, 640, 360, VideoCodec::H264)?;
    ensure!(
        cuda.ctx_get_current()? == caller_context,
        "NVDEC constructor changed the caller CUDA context"
    );
    let mut frames = 0;
    for (width, height) in [
        (640, 360),
        (1920, 1080),
        (1280, 720),
        (1384, 904),
        (640, 360),
    ] {
        encoder.resize(width, height)?;
        let frame = Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            data: [0, 0, 255, 255].repeat((width * height) as usize),
            timestamp: Instant::now(),
        };
        for _ in 0..2 {
            let packet = encoder.encode_frame(&frame)?;
            let pixels = decoder.decode_frame(&packet.data)?;
            ensure!(
                decoder.dimensions() == (width, height),
                "stale NVDEC dimensions: {:?}, expected {width}x{height}",
                decoder.dimensions()
            );
            ensure!(
                pixels.len() == (width * height) as usize,
                "missing or truncated output: {} pixels for {width}x{height}",
                pixels.len()
            );
            ensure!(
                pixels
                    .iter()
                    .step_by(97)
                    .all(|p| ((p >> 16) & 255) > 220 && ((p >> 8) & 255) < 35 && (p & 255) < 35),
                "decoded red fixture has corrupt colors at {width}x{height}"
            );
            frames += 1;
        }
        println!("PASS {width}x{height}: two complete frames, valid red pixels");
    }
    drop(decoder);
    ensure!(
        cuda.ctx_get_current()? == caller_context,
        "NVDEC destruction changed the caller CUDA context"
    );
    println!("NVDEC regression passed: {frames} frames across five resolutions");
    Ok(())
}
