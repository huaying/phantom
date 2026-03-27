//! Mock server that generates fake frames for testing the client.
//! No screen capture needed — works without any OS permissions.

use anyhow::Result;
use phantom_core::encode::{EncodedTile, TileEncoding};
use phantom_core::frame::PixelFormat;
use phantom_core::protocol::{self, Message};
use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;
const TILE_SIZE: u32 = 64;
const FPS: u32 = 30;

fn main() -> Result<()> {
    eprintln!("Mock server listening on 0.0.0.0:9900 ({}x{})", WIDTH, HEIGHT);
    let listener = TcpListener::bind("0.0.0.0:9900")?;

    let (mut stream, addr) = listener.accept()?;
    stream.set_nodelay(true)?;
    eprintln!("Client connected from {addr}");

    // Send Hello
    protocol::write_message(
        &mut stream,
        &Message::Hello {
            width: WIDTH,
            height: HEIGHT,
            format: PixelFormat::Bgra8,
        },
    )?;

    let tiles_x = (WIDTH + TILE_SIZE - 1) / TILE_SIZE;
    let tiles_y = (HEIGHT + TILE_SIZE - 1) / TILE_SIZE;
    let frame_interval = Duration::from_secs_f64(1.0 / FPS as f64);

    let mut sequence = 0u64;
    let start = Instant::now();

    loop {
        let loop_start = Instant::now();
        let t = start.elapsed().as_secs_f32();
        sequence += 1;

        let mut tiles = Vec::new();

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let tw = (WIDTH - tx * TILE_SIZE).min(TILE_SIZE);
                let th = (HEIGHT - ty * TILE_SIZE).min(TILE_SIZE);

                let mut data = Vec::with_capacity((tw * th * 4) as usize);
                for py in 0..th {
                    for px in 0..tw {
                        let gx = tx * TILE_SIZE + px;
                        let gy = ty * TILE_SIZE + py;

                        // Animated gradient + moving circle
                        let fx = gx as f32 / WIDTH as f32;
                        let fy = gy as f32 / HEIGHT as f32;

                        // Circle position (bouncing)
                        let cx = 0.5 + 0.3 * (t * 0.7).sin();
                        let cy = 0.5 + 0.3 * (t * 1.1).cos();
                        let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                        let in_circle = dist < 0.1;

                        let (r, g, b) = if in_circle {
                            // White circle
                            (240u8, 240u8, 240u8)
                        } else {
                            // Animated gradient background
                            let phase = t * 0.5;
                            let r = ((fx * 200.0 + phase * 50.0) % 255.0) as u8;
                            let g = ((fy * 200.0) % 255.0) as u8;
                            let b = (((fx + fy) * 100.0 + phase * 30.0) % 255.0) as u8;
                            (r, g, b)
                        };

                        // BGRA
                        data.push(b);
                        data.push(g);
                        data.push(r);
                        data.push(255);
                    }
                }

                // Compress with zstd
                let compressed = zstd::encode_all(data.as_slice(), 1)?;
                let (final_data, encoding) = if compressed.len() < data.len() {
                    (compressed, TileEncoding::Zstd)
                } else {
                    (data, TileEncoding::Raw)
                };

                tiles.push(EncodedTile {
                    tile_x: tx,
                    tile_y: ty,
                    pixel_width: tw,
                    pixel_height: th,
                    encoding,
                    data: final_data,
                });
            }
        }

        if let Err(e) = protocol::write_message(
            &mut stream,
            &Message::FrameUpdate { sequence, tiles },
        ) {
            eprintln!("Client disconnected: {e}");
            break;
        }
        stream.flush()?;

        if sequence % 150 == 0 {
            eprintln!("Sent {sequence} frames ({:.0}s)", t);
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    Ok(())
}
