use crate::cursor::LocalCursor;
use anyhow::{Context, Result};
use minifb::{MouseMode, Window, WindowOptions};
use phantom_core::decode::DecodedTile;
use phantom_core::display::Display;
use phantom_core::tile::TILE_SIZE;

pub struct MinifbDisplay {
    window: Option<Window>,
    buffer: Vec<u32>,
    server_width: u32,
    server_height: u32,
    window_width: u32,
    window_height: u32,
    cursor: LocalCursor,
}

impl MinifbDisplay {
    pub fn new() -> Self {
        Self {
            window: None,
            buffer: Vec::new(),
            server_width: 0,
            server_height: 0,
            window_width: 0,
            window_height: 0,
            cursor: LocalCursor::new(),
        }
    }

    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    pub fn update_full_frame(&mut self, rgb32: &[u32]) {
        let len = self.buffer.len().min(rgb32.len());
        self.buffer[..len].copy_from_slice(&rgb32[..len]);
    }

    /// Map mouse coordinates to server space.
    /// minifb with ScaleMode::AspectRatioStretch + get_mouse_pos(Clamp) returns
    /// coordinates in buffer (server) space already. So we just clamp, no scaling.
    pub fn map_mouse(&self, x: f32, y: f32) -> (i32, i32) {
        let sx = (x as i32).clamp(0, self.server_width as i32 - 1);
        let sy = (y as i32).clamp(0, self.server_height as i32 - 1);
        (sx, sy)
    }
}

impl Display for MinifbDisplay {
    fn init(&mut self, width: u32, height: u32) -> Result<()> {
        self.server_width = width;
        self.server_height = height;
        self.buffer = vec![0u32; (width * height) as usize];

        // Scale window to fit ~80% of the client's screen height.
        // minifb's ScaleMode::AspectRatioStretch handles the rendering.
        let (win_w, win_h) = fit_to_screen(width, height, 0.8);
        self.window_width = win_w;
        self.window_height = win_h;

        let window = Window::new(
            "Phantom Remote Desktop",
            win_w as usize,
            win_h as usize,
            WindowOptions {
                resize: true,
                scale_mode: minifb::ScaleMode::AspectRatioStretch,
                ..WindowOptions::default()
            },
        )
        .context("failed to create window")?;

        self.window = Some(window);
        tracing::info!(
            server = format_args!("{}x{}", width, height),
            window = format_args!("{}x{}", win_w, win_h),
            "display initialized"
        );
        Ok(())
    }

    fn update_tiles(&mut self, tiles: &[DecodedTile]) -> Result<()> {
        let bpp = 4;
        let w = self.server_width as usize;

        for tile in tiles {
            let base_x = (tile.tile_x * TILE_SIZE) as usize;
            let base_y = (tile.tile_y * TILE_SIZE) as usize;
            let tw = tile.pixel_width as usize;
            let th = tile.pixel_height as usize;

            // Validate tile bounds
            if base_x >= w || base_y >= self.server_height as usize {
                continue;
            }
            let expected_data = tw * th * bpp;
            if tile.data.len() < expected_data {
                tracing::debug!("tile data too short ({} < {}), skipping", tile.data.len(), expected_data);
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
                    self.buffer[dst_offset + col] =
                        ((tile.data[si + 2] as u32) << 16)
                        | ((tile.data[si + 1] as u32) << 8)
                        | (tile.data[si] as u32);
                }
            }
        }
        Ok(())
    }

    fn present(&mut self) -> Result<bool> {
        let window = match self.window.as_mut() {
            Some(w) => w,
            None => anyhow::bail!("display not initialized"),
        };
        if !window.is_open() {
            return Ok(false);
        }

        // Track window resize
        let (cur_w, cur_h) = window.get_size();
        if cur_w as u32 != self.window_width || cur_h as u32 != self.window_height {
            self.window_width = cur_w as u32;
            self.window_height = cur_h as u32;
        }

        // Draw local cursor at mouse position (already in server/buffer coordinates)
        let cursor_pos = window.get_mouse_pos(MouseMode::Clamp).map(|(x, y)| {
            (x as i32, y as i32)
        });

        // Undraw previous cursor, draw new one
        self.cursor.undraw(&mut self.buffer);
        if let Some((cx, cy)) = cursor_pos {
            self.cursor.draw(
                &mut self.buffer,
                self.server_width as usize,
                self.server_height as usize,
                cx,
                cy,
            );
        }

        window
            .update_with_buffer(
                &self.buffer,
                self.server_width as usize,
                self.server_height as usize,
            )
            .context("display update failed")?;
        Ok(true)
    }
}

/// Calculate a window size that fits within `fraction` of the primary screen.
fn fit_to_screen(server_w: u32, server_h: u32, fraction: f32) -> (u32, u32) {
    // Try to detect screen size. If we can't, fall back to server resolution.
    let (screen_w, screen_h) = detect_screen_size().unwrap_or((server_w, server_h));

    let max_w = (screen_w as f32 * fraction) as u32;
    let max_h = (screen_h as f32 * fraction) as u32;

    if server_w <= max_w && server_h <= max_h {
        // Server resolution fits, use it directly
        return (server_w, server_h);
    }

    // Scale down maintaining aspect ratio
    let scale = (max_w as f32 / server_w as f32).min(max_h as f32 / server_h as f32);
    let win_w = (server_w as f32 * scale) as u32;
    let win_h = (server_h as f32 * scale) as u32;
    (win_w.max(320), win_h.max(240))
}

fn detect_screen_size() -> Option<(u32, u32)> {
    // Use scrap to detect primary display size (works cross-platform)
    scrap::Display::primary().ok().map(|d| (d.width() as u32, d.height() as u32))
}
