use anyhow::{Context, Result};
use minifb::{Window, WindowOptions};
use phantom_core::decode::DecodedTile;
use phantom_core::display::Display;
use phantom_core::tile::TILE_SIZE;

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

    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    /// Replace entire framebuffer with decoded H.264 frame (0RGB u32 format).
    pub fn update_full_frame(&mut self, rgb32: &[u32]) {
        let len = self.buffer.len().min(rgb32.len());
        self.buffer[..len].copy_from_slice(&rgb32[..len]);
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
        let bpp = 4;
        let w = self.width as usize;

        for tile in tiles {
            let base_x = (tile.tile_x * TILE_SIZE) as usize;
            let base_y = (tile.tile_y * TILE_SIZE) as usize;
            let tw = tile.pixel_width as usize;
            let th = tile.pixel_height as usize;

            for row in 0..th {
                let dst_y = base_y + row;
                if dst_y >= self.height as usize {
                    break;
                }

                let src_offset = row * tw * bpp;
                let dst_offset = dst_y * w + base_x;
                let copy_w = tw.min(w - base_x);

                for col in 0..copy_w {
                    let si = src_offset + col * bpp;
                    let b = tile.data[si] as u32;
                    let g = tile.data[si + 1] as u32;
                    let r = tile.data[si + 2] as u32;
                    self.buffer[dst_offset + col] = (r << 16) | (g << 8) | b;
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
