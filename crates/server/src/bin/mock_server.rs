//! Mock server: generates animated H.264 frames. No screen capture needed.

use anyhow::Result;
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::YUVBuffer;
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::frame::PixelFormat;
use phantom_core::protocol::{self, Message};
use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

struct BgraFrame<'a>(&'a [u8], usize, usize);
impl openh264::formats::RGBSource for BgraFrame<'_> {
    fn dimensions(&self) -> (usize, usize) { (self.1, self.2) }
    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let i = (y * self.1 + x) * 4;
        (self.0[i+2] as f32, self.0[i+1] as f32, self.0[i] as f32)
    }
}

const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;
const FPS: u32 = 30;
const BITRATE_KBPS: u32 = 3000;

fn main() -> Result<()> {
    eprintln!(
        "Mock server on 0.0.0.0:9900 ({}x{}, H.264 {}kbps)",
        WIDTH, HEIGHT, BITRATE_KBPS
    );
    let listener = TcpListener::bind("0.0.0.0:9900")?;
    let (mut stream, addr) = listener.accept()?;
    stream.set_nodelay(true)?;
    eprintln!("Client connected from {addr}");

    protocol::write_message(
        &mut stream,
        &Message::Hello {
            width: WIDTH,
            height: HEIGHT,
            format: PixelFormat::Bgra8,
        },
    )?;

    let config = EncoderConfig::new()
        .max_frame_rate(FPS as f32)
        .set_bitrate_bps(BITRATE_KBPS * 1000);
    let api = openh264::OpenH264API::from_source();
    let mut encoder = Encoder::with_api_config(api, config)?;

    let frame_interval = Duration::from_secs_f64(1.0 / FPS as f64);
    let mut sequence = 0u64;
    let start = Instant::now();
    let mut total_bytes: u64 = 0;

    loop {
        let loop_start = Instant::now();
        let t = start.elapsed().as_secs_f32();
        sequence += 1;

        let bgra = generate_frame(WIDTH, HEIGHT, t);
        let yuv = YUVBuffer::from_rgb_source(BgraFrame(&bgra, WIDTH as usize, HEIGHT as usize));
        let bitstream = encoder.encode(&yuv)?;
        let h264_data = bitstream.to_vec();
        total_bytes += h264_data.len() as u64;

        let msg = Message::VideoFrame {
            sequence,
            frame: Box::new(EncodedFrame {
                codec: VideoCodec::H264,
                data: h264_data,
                is_keyframe: sequence == 1,
            }),
        };

        if let Err(e) = protocol::write_message(&mut stream, &msg) {
            eprintln!("Client disconnected: {e}");
            break;
        }
        stream.flush()?;

        if sequence.is_multiple_of(150) {
            let elapsed = start.elapsed().as_secs_f64();
            eprintln!(
                "Frame {sequence} | {:.0}s | avg {:.1} KB/s",
                elapsed,
                total_bytes as f64 / elapsed / 1024.0
            );
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    Ok(())
}

fn generate_frame(width: u32, height: u32, t: f32) -> Vec<u8> {
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    let cx = 0.5 + 0.3 * (t * 0.7).sin();
    let cy = 0.5 + 0.3 * (t * 1.1).cos();

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();

            let (r, g, b) = if dist < 0.08 {
                (255u8, 255, 255)
            } else {
                let phase = t * 0.3;
                (
                    ((fx * 200.0 + phase * 50.0) % 255.0) as u8,
                    ((fy * 180.0) % 255.0) as u8,
                    (((fx + fy) * 100.0 + phase * 30.0) % 255.0) as u8,
                )
            };

            let idx = ((y * width + x) * 4) as usize;
            bgra[idx] = b;
            bgra[idx + 1] = g;
            bgra[idx + 2] = r;
            bgra[idx + 3] = 255;
        }
    }
    bgra
}
