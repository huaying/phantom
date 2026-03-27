use crate::frame::Frame;

pub const TILE_SIZE: u32 = 64;

#[derive(Debug, Clone)]
pub struct DirtyTile {
    /// Tile grid coordinate (not pixel coordinate).
    pub tile_x: u32,
    pub tile_y: u32,
    /// Actual pixel dimensions (edge tiles may be smaller than TILE_SIZE).
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Raw pixel data for this tile, row-major, same format as source frame.
    pub data: Vec<u8>,
}

/// Detects which tiles changed between consecutive frames.
///
/// Uses simple byte comparison. Future: GPU compute shader for faster diff.
pub struct TileDiffer {
    prev_data: Option<Vec<u8>>,
    width: u32,
    height: u32,
}

impl TileDiffer {
    pub fn new() -> Self {
        Self {
            prev_data: None,
            width: 0,
            height: 0,
        }
    }

    /// Compare `frame` against the previous frame, return list of changed tiles.
    /// First call always returns all tiles as dirty.
    pub fn diff(&mut self, frame: &Frame) -> Vec<DirtyTile> {
        let bpp = frame.format.bytes_per_pixel();
        let stride = frame.stride();
        let tiles_x = (frame.width + TILE_SIZE - 1) / TILE_SIZE;
        let tiles_y = (frame.height + TILE_SIZE - 1) / TILE_SIZE;

        // If resolution changed, treat everything as dirty.
        let resolution_changed =
            self.width != frame.width || self.height != frame.height;

        let mut dirty = Vec::new();

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let tile_w = (frame.width - tx * TILE_SIZE).min(TILE_SIZE);
                let tile_h = (frame.height - ty * TILE_SIZE).min(TILE_SIZE);
                let tile_x_px = (tx * TILE_SIZE) as usize;
                let tile_y_px = (ty * TILE_SIZE) as usize;

                let changed = if resolution_changed || self.prev_data.is_none() {
                    true
                } else {
                    let prev = self.prev_data.as_ref().unwrap();
                    tile_changed(
                        &frame.data, prev, stride, bpp,
                        tile_x_px, tile_y_px,
                        tile_w as usize, tile_h as usize,
                    )
                };

                if changed {
                    let tile_data = extract_tile(
                        &frame.data, stride, bpp,
                        tile_x_px, tile_y_px,
                        tile_w as usize, tile_h as usize,
                    );
                    dirty.push(DirtyTile {
                        tile_x: tx,
                        tile_y: ty,
                        pixel_width: tile_w,
                        pixel_height: tile_h,
                        data: tile_data,
                    });
                }
            }
        }

        self.prev_data = Some(frame.data.clone());
        self.width = frame.width;
        self.height = frame.height;

        dirty
    }
}

fn tile_changed(
    curr: &[u8], prev: &[u8], stride: usize, bpp: usize,
    x_px: usize, y_px: usize, w: usize, h: usize,
) -> bool {
    let row_bytes = w * bpp;
    for row in 0..h {
        let y = y_px + row;
        let offset = y * stride + x_px * bpp;
        if curr[offset..offset + row_bytes] != prev[offset..offset + row_bytes] {
            return true;
        }
    }
    false
}

fn extract_tile(
    data: &[u8], stride: usize, bpp: usize,
    x_px: usize, y_px: usize, w: usize, h: usize,
) -> Vec<u8> {
    let row_bytes = w * bpp;
    let mut out = Vec::with_capacity(row_bytes * h);
    for row in 0..h {
        let y = y_px + row;
        let offset = y * stride + x_px * bpp;
        out.extend_from_slice(&data[offset..offset + row_bytes]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PixelFormat;
    use std::time::Instant;

    fn make_frame(width: u32, height: u32, fill: u8) -> Frame {
        let bpp = 4;
        let data = vec![fill; (width * height) as usize * bpp];
        Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            data,
            timestamp: Instant::now(),
        }
    }

    #[test]
    fn first_frame_all_dirty() {
        let mut differ = TileDiffer::new();
        let frame = make_frame(128, 128, 0);
        let dirty = differ.diff(&frame);
        // 128/64 = 2x2 = 4 tiles
        assert_eq!(dirty.len(), 4);
    }

    #[test]
    fn identical_frame_no_dirty() {
        let mut differ = TileDiffer::new();
        let frame = make_frame(128, 128, 42);
        differ.diff(&frame);
        let dirty = differ.diff(&frame);
        assert_eq!(dirty.len(), 0);
    }

    #[test]
    fn one_pixel_change_one_dirty_tile() {
        let mut differ = TileDiffer::new();
        let frame1 = make_frame(128, 128, 0);
        differ.diff(&frame1);

        let mut frame2 = make_frame(128, 128, 0);
        // Change one pixel in tile (1, 0) — pixel at (64, 0)
        let bpp = 4;
        let offset = 0 * (128 * bpp) + 64 * bpp; // row 0, col 64
        frame2.data[offset] = 255;

        let dirty = differ.diff(&frame2);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].tile_x, 1);
        assert_eq!(dirty[0].tile_y, 0);
    }

    #[test]
    fn non_aligned_resolution() {
        let mut differ = TileDiffer::new();
        // 100x100 → tiles: ceil(100/64)=2 x ceil(100/64)=2 = 4 tiles
        // Edge tiles: width 36, height 36
        let frame = make_frame(100, 100, 0);
        let dirty = differ.diff(&frame);
        assert_eq!(dirty.len(), 4);

        // Check edge tile dimensions
        let bottom_right = dirty.iter().find(|t| t.tile_x == 1 && t.tile_y == 1).unwrap();
        assert_eq!(bottom_right.pixel_width, 36);
        assert_eq!(bottom_right.pixel_height, 36);
    }

    #[test]
    fn tile_data_correctness() {
        let mut differ = TileDiffer::new();
        let mut frame = make_frame(128, 64, 0);
        // Fill tile (1,0) area with 0xFF
        let bpp = 4;
        for row in 0..64u32 {
            for col in 64..128u32 {
                let offset = (row * 128 + col) as usize * bpp;
                frame.data[offset..offset + bpp].fill(0xFF);
            }
        }
        let dirty = differ.diff(&frame);
        let tile_1_0 = dirty.iter().find(|t| t.tile_x == 1 && t.tile_y == 0).unwrap();
        assert!(tile_1_0.data.iter().all(|&b| b == 0xFF));
    }
}
