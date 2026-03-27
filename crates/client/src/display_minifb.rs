use anyhow::{Context, Result};
use minifb::{Window, WindowOptions};
use phantom_core::decode::DecodedTile;
use phantom_core::display::Display;
use phantom_core::tile::TILE_SIZE;

/// CPU-based display using minifb (simple framebuffer window).
pub struct MinifbDisplay {
    window: Option<Window>,
    buffer: Vec<u32>,
    width: u32,
    height: u32,
}

impl MinifbDisplay {
    pub fn new() -> Self {
        Self {
            window: None,
            buffer: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// Access the underlying window (for input capture).
    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }
}

impl Display for MinifbDisplay {
    fn init(&mut self, width: u32, height: u32) -> Result<()> {
        self.width = width;
        self.height = height;
        self.buffer = vec![0u32; (width * height) as usize];

        let window = Window::new(
            "Phantom Remote Desktop",
            width as usize,
            height as usize,
            WindowOptions {
                resize: false,
                scale_mode: minifb::ScaleMode::AspectRatioStretch,
                ..WindowOptions::default()
            },
        )
        .context("failed to create window")?;

        self.window = Some(window);
        tracing::info!(width, height, "display initialized");
        Ok(())
    }

    fn update_tiles(&mut self, tiles: &[DecodedTile]) -> Result<()> {
        let bpp = 4; // BGRA
        for tile in tiles {
            let base_x = tile.tile_x * TILE_SIZE;
            let base_y = tile.tile_y * TILE_SIZE;

            for row in 0..tile.pixel_height {
                for col in 0..tile.pixel_width {
                    let src_idx = (row * tile.pixel_width + col) as usize * bpp;
                    let dst_x = (base_x + col) as usize;
                    let dst_y = (base_y + row) as usize;

                    if dst_x >= self.width as usize || dst_y >= self.height as usize {
                        continue;
                    }

                    let dst_idx = dst_y * self.width as usize + dst_x;

                    // BGRA → 0RGB
                    let b = tile.data[src_idx] as u32;
                    let g = tile.data[src_idx + 1] as u32;
                    let r = tile.data[src_idx + 2] as u32;
                    self.buffer[dst_idx] = (r << 16) | (g << 8) | b;
                }
            }
        }
        Ok(())
    }

    fn present(&mut self) -> Result<bool> {
        let window = self.window.as_mut().unwrap();
        if !window.is_open() {
            return Ok(false);
        }
        window
            .update_with_buffer(&self.buffer, self.width as usize, self.height as usize)
            .context("display update failed")?;
        Ok(true)
    }
}
