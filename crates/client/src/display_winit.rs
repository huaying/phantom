use anyhow::{Context, Result};
use phantom_core::decode::DecodedTile;
use phantom_core::tile::TILE_SIZE;
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::window::Window;

/// Display using winit + softbuffer.
pub struct WinitDisplay {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    buffer: Vec<u32>,
    server_width: u32,
    server_height: u32,
}

impl WinitDisplay {
    pub fn new(window: Rc<Window>, server_width: u32, server_height: u32) -> Result<Self> {
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("create context: {e}"))?;
        let surface = Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("create surface: {e}"))?;

        let buffer = vec![0u32; (server_width * server_height) as usize];

        tracing::info!(
            server = format_args!("{}x{}", server_width, server_height),
            "display initialized"
        );

        Ok(Self {
            window,
            surface,
            buffer,
            server_width,
            server_height,
        })
    }

    /// Replace entire framebuffer with decoded H.264 frame.
    pub fn update_full_frame(&mut self, rgb32: &[u32]) {
        let len = self.buffer.len().min(rgb32.len());
        self.buffer[..len].copy_from_slice(&rgb32[..len]);
    }

    /// Apply decoded lossless tiles to the framebuffer.
    pub fn update_tiles(&mut self, tiles: &[DecodedTile]) {
        let bpp = 4;
        let w = self.server_width as usize;

        for tile in tiles {
            let base_x = (tile.tile_x * TILE_SIZE) as usize;
            let base_y = (tile.tile_y * TILE_SIZE) as usize;
            let tw = tile.pixel_width as usize;
            let th = tile.pixel_height as usize;

            if base_x >= w || base_y >= self.server_height as usize {
                continue;
            }
            let expected_data = tw * th * bpp;
            if tile.data.len() < expected_data {
                continue;
            }

            for row in 0..th {
                let dst_y = base_y + row;
                if dst_y >= self.server_height as usize {
                    break;
                }
                let src_offset = row * tw * bpp;
                let dst_offset = dst_y * w + base_x;
                let copy_w = tw.min(w.saturating_sub(base_x));

                for col in 0..copy_w {
                    let si = src_offset + col * bpp;
                    self.buffer[dst_offset + col] = ((tile.data[si + 2] as u32) << 16)
                        | ((tile.data[si + 1] as u32) << 8)
                        | (tile.data[si] as u32);
                }
            }
        }
    }

    /// Draw local cursor and present framebuffer to screen.
    pub fn present(&mut self, cursor_pos: Option<PhysicalPosition<f64>>) -> Result<()> {
        let w = self.server_width;
        let h = self.server_height;

        // Resize surface to match server resolution
        self.surface
            .resize(
                NonZeroU32::new(w).context("zero width")?,
                NonZeroU32::new(h).context("zero height")?,
            )
            .map_err(|e| anyhow::anyhow!("resize surface: {e}"))?;

        let mut sb = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("get buffer: {e}"))?;

        // Copy our framebuffer to the surface
        let len = sb.len().min(self.buffer.len());
        sb[..len].copy_from_slice(&self.buffer[..len]);

        // Draw cursor overlay
        if let Some(pos) = cursor_pos {
            draw_cursor(&mut sb, w as usize, h as usize, pos.x as i32, pos.y as i32);
        }

        sb.present()
            .map_err(|e| anyhow::anyhow!("present: {e}"))?;
        Ok(())
    }

    /// Map physical cursor position to server coordinates.
    pub fn map_to_server(&self, pos: PhysicalPosition<f64>) -> (i32, i32) {
        let win_size = self.window.inner_size();
        let scale_x = self.server_width as f64 / win_size.width as f64;
        let scale_y = self.server_height as f64 / win_size.height as f64;
        (
            (pos.x * scale_x).clamp(0.0, self.server_width as f64 - 1.0) as i32,
            (pos.y * scale_y).clamp(0.0, self.server_height as f64 - 1.0) as i32,
        )
    }
}

/// Calculate a window size that fits within ~80% of the screen.
pub fn fit_window_size(server_w: u32, server_h: u32) -> LogicalSize<u32> {
    let (screen_w, screen_h) = scrap::Display::primary()
        .map(|d| (d.width() as u32, d.height() as u32))
        .unwrap_or((server_w, server_h));

    let max_w = (screen_w as f32 * 0.8) as u32;
    let max_h = (screen_h as f32 * 0.8) as u32;

    if server_w <= max_w && server_h <= max_h {
        return LogicalSize::new(server_w, server_h);
    }

    let scale = (max_w as f32 / server_w as f32).min(max_h as f32 / server_h as f32);
    LogicalSize::new(
        (server_w as f32 * scale).max(320.0) as u32,
        (server_h as f32 * scale).max(240.0) as u32,
    )
}

// -- Cursor bitmap (same as before) --

const CURSOR_W: usize = 12;
const CURSOR_H: usize = 19;

#[rustfmt::skip]
const CURSOR_BITMAP: [u8; CURSOR_W * CURSOR_H] = [
    2,0,0,0,0,0,0,0,0,0,0,0,
    2,2,0,0,0,0,0,0,0,0,0,0,
    2,1,2,0,0,0,0,0,0,0,0,0,
    2,1,1,2,0,0,0,0,0,0,0,0,
    2,1,1,1,2,0,0,0,0,0,0,0,
    2,1,1,1,1,2,0,0,0,0,0,0,
    2,1,1,1,1,1,2,0,0,0,0,0,
    2,1,1,1,1,1,1,2,0,0,0,0,
    2,1,1,1,1,1,1,1,2,0,0,0,
    2,1,1,1,1,1,1,1,1,2,0,0,
    2,1,1,1,1,1,1,1,1,1,2,0,
    2,1,1,1,1,1,1,1,1,1,1,2,
    2,1,1,1,1,1,2,2,2,2,2,2,
    2,1,1,1,2,1,2,0,0,0,0,0,
    2,1,1,2,0,2,1,2,0,0,0,0,
    2,1,2,0,0,2,1,2,0,0,0,0,
    2,2,0,0,0,0,2,1,2,0,0,0,
    2,0,0,0,0,0,2,1,2,0,0,0,
    0,0,0,0,0,0,0,2,0,0,0,0,
];

fn draw_cursor(buffer: &mut [u32], buf_w: usize, buf_h: usize, x: i32, y: i32) {
    for cy in 0..CURSOR_H {
        for cx in 0..CURSOR_W {
            let px = x as isize + cx as isize;
            let py = y as isize + cy as isize;
            if px < 0 || py < 0 || px >= buf_w as isize || py >= buf_h as isize {
                continue;
            }
            let val = CURSOR_BITMAP[cy * CURSOR_W + cx];
            if val == 0 {
                continue;
            }
            let idx = py as usize * buf_w + px as usize;
            buffer[idx] = match val {
                1 => 0x00FFFFFF,
                2 => 0x00000000,
                _ => continue,
            };
        }
    }
}
