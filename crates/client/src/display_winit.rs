use anyhow::{Context, Result};
use softbuffer::Surface;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::window::Window;

/// Display using winit + softbuffer.
pub struct WinitDisplay {
    pub window: Rc<Window>,
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
    /// Skips if frame size doesn't match buffer (resolution change in progress).
    pub fn update_full_frame(&mut self, rgb32: &[u32]) {
        let expected = (self.server_width * self.server_height) as usize;
        if rgb32.len() != expected {
            return; // Skip mismatched frame during resolution transition
        }
        self.buffer.copy_from_slice(rgb32);
    }

    pub fn server_width(&self) -> u32 {
        self.server_width
    }

    pub fn server_height(&self) -> u32 {
        self.server_height
    }

    /// Resize the framebuffer for a new server resolution.
    /// Called when decoder detects resolution change from SPS/PPS.
    pub fn resize_server(&mut self, width: u32, height: u32) {
        if width == self.server_width && height == self.server_height {
            return;
        }
        tracing::info!(
            old = format_args!("{}x{}", self.server_width, self.server_height),
            new = format_args!("{}x{}", width, height),
            "display: resizing framebuffer"
        );
        self.server_width = width;
        self.server_height = height;
        self.buffer = vec![0u32; (width * height) as usize];
    }

    /// Draw local cursor and present framebuffer to screen.
    /// If window is smaller than server resolution, downscale with nearest-neighbor.
    pub fn present(&mut self, cursor_pos: Option<PhysicalPosition<f64>>) -> Result<()> {
        let win_size = self.window.inner_size();
        let dst_w = win_size.width;
        let dst_h = win_size.height;

        if dst_w == 0 || dst_h == 0 {
            return Ok(());
        }

        self.surface
            .resize(
                NonZeroU32::new(dst_w).context("zero width")?,
                NonZeroU32::new(dst_h).context("zero height")?,
            )
            .map_err(|e| anyhow::anyhow!("resize surface: {e}"))?;

        let mut sb = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("get buffer: {e}"))?;

        let src_w = self.server_width as usize;
        let src_h = self.server_height as usize;

        if dst_w as usize == src_w && dst_h as usize == src_h {
            // No scaling needed — direct copy
            let len = sb.len().min(self.buffer.len());
            sb[..len].copy_from_slice(&self.buffer[..len]);
        } else {
            // Maintain aspect ratio with letterboxing
            let src_aspect = src_w as f64 / src_h as f64;
            let dst_aspect = dst_w as f64 / dst_h as f64;
            let (render_w, render_h, offset_x, offset_y) = if src_aspect > dst_aspect {
                // Pillarbox (black bars top/bottom)
                let rw = dst_w as usize;
                let rh = (dst_w as f64 / src_aspect) as usize;
                (rw, rh, 0, (dst_h as usize - rh) / 2)
            } else {
                // Letterbox (black bars left/right)
                let rh = dst_h as usize;
                let rw = (dst_h as f64 * src_aspect) as usize;
                (rw, rh, (dst_w as usize - rw) / 2, 0)
            };

            // Clear to black
            sb.fill(0);

            // Bilinear downscale into the render area
            for dy in 0..render_h {
                let sy_f = dy as f64 * (src_h - 1) as f64 / (render_h - 1).max(1) as f64;
                let sy0 = sy_f as usize;
                let sy1 = (sy0 + 1).min(src_h - 1);
                let fy = sy_f - sy0 as f64;

                for dx in 0..render_w {
                    let sx_f = dx as f64 * (src_w - 1) as f64 / (render_w - 1).max(1) as f64;
                    let sx0 = sx_f as usize;
                    let sx1 = (sx0 + 1).min(src_w - 1);
                    let fx = sx_f - sx0 as f64;

                    let p00 = self.buffer[sy0 * src_w + sx0];
                    let p10 = self.buffer[sy0 * src_w + sx1];
                    let p01 = self.buffer[sy1 * src_w + sx0];
                    let p11 = self.buffer[sy1 * src_w + sx1];

                    let pixel = bilinear(p00, p10, p01, p11, fx, fy);
                    let dst_idx = (offset_y + dy) * dst_w as usize + (offset_x + dx);
                    if dst_idx < sb.len() {
                        sb[dst_idx] = pixel;
                    }
                }
            }
        }

        // Draw cursor overlay
        if let Some(pos) = cursor_pos {
            draw_cursor(
                &mut sb,
                dst_w as usize,
                dst_h as usize,
                pos.x as i32,
                pos.y as i32,
            );
        }

        // macOS: fade the top strip to black so the traffic-light buttons
        // (which sit on top of the video because the title bar is
        // transparent+fullsize_content_view) have a readable backdrop
        // regardless of what colour the remote desktop is showing.
        #[cfg(target_os = "macos")]
        draw_top_gradient(&mut sb, dst_w as usize, dst_h as usize);

        sb.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        Ok(())
    }

    /// Map physical cursor position to server coordinates, accounting for
    /// aspect-ratio letterboxing.
    pub fn map_to_server(&self, pos: PhysicalPosition<f64>) -> (i32, i32) {
        let win_size = self.window.inner_size();
        let dst_w = win_size.width as f64;
        let dst_h = win_size.height as f64;
        let src_w = self.server_width as f64;
        let src_h = self.server_height as f64;

        if dst_w == 0.0 || dst_h == 0.0 {
            return (0, 0);
        }

        let src_aspect = src_w / src_h;
        let dst_aspect = dst_w / dst_h;
        let (render_w, render_h, offset_x, offset_y) = if src_aspect > dst_aspect {
            let rw = dst_w;
            let rh = dst_w / src_aspect;
            (rw, rh, 0.0, (dst_h - rh) / 2.0)
        } else {
            let rh = dst_h;
            let rw = dst_h * src_aspect;
            (rw, rh, (dst_w - rw) / 2.0, 0.0)
        };

        let x = ((pos.x - offset_x) / render_w * src_w).clamp(0.0, src_w - 1.0) as i32;
        let y = ((pos.y - offset_y) / render_h * src_h).clamp(0.0, src_h - 1.0) as i32;
        (x, y)
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

/// Darken the top ~50px of the framebuffer with a vertical gradient: full
/// black at y=0 fading to the original pixel at y=GRAD_H. Gives the macOS
/// traffic lights a legible backdrop over any remote-desktop content.
#[cfg(target_os = "macos")]
fn draw_top_gradient(buffer: &mut [u32], buf_w: usize, buf_h: usize) {
    const GRAD_H: usize = 50;
    let h = GRAD_H.min(buf_h);
    for y in 0..h {
        // Scale factor for the original pixel: 0 (fully black) at y=0,
        // 1 (untouched) at y=h.
        let alpha = y as f32 / h as f32;
        let row = y * buf_w;
        for x in 0..buf_w {
            let idx = row + x;
            let p = buffer[idx];
            let r = (((p >> 16) & 0xff) as f32 * alpha) as u32;
            let g = (((p >> 8) & 0xff) as f32 * alpha) as u32;
            let b = ((p & 0xff) as f32 * alpha) as u32;
            buffer[idx] = (r << 16) | (g << 8) | b;
        }
    }
}

/// Bilinear interpolation of four pixels.
fn bilinear(p00: u32, p10: u32, p01: u32, p11: u32, fx: f64, fy: f64) -> u32 {
    let mix = |c0: u32, c1: u32, c2: u32, c3: u32, shift: u32| -> u32 {
        let v00 = ((c0 >> shift) & 0xFF) as f64;
        let v10 = ((c1 >> shift) & 0xFF) as f64;
        let v01 = ((c2 >> shift) & 0xFF) as f64;
        let v11 = ((c3 >> shift) & 0xFF) as f64;
        let top = v00 + (v10 - v00) * fx;
        let bot = v01 + (v11 - v01) * fx;
        (top + (bot - top) * fy).clamp(0.0, 255.0) as u32
    };
    (mix(p00, p10, p01, p11, 16) << 16)
        | (mix(p00, p10, p01, p11, 8) << 8)
        | mix(p00, p10, p01, p11, 0)
}

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
