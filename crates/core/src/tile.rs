use crate::frame::Frame;

pub const TILE_SIZE: u32 = 64;

#[derive(Debug, Clone)]
pub struct DirtyTile {
    pub tile_x: u32,
    pub tile_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub data: Vec<u8>,
}

/// Detects which tiles changed between consecutive frames.
pub struct TileDiffer {
    prev_data: Vec<u8>,
    width: u32,
    height: u32,
    initialized: bool,
}

impl TileDiffer {
    pub fn new() -> Self {
        Self {
            prev_data: Vec::new(),
            width: 0,
            height: 0,
            initialized: false,
        }
    }

    /// Quick check: did anything change at all? (Cheap — samples ~64 points.)
    pub fn has_changes(&self, frame: &Frame) -> bool {
        if !self.initialized
            || self.width != frame.width
            || self.height != frame.height
        {
            return true;
        }

        let len = frame.data.len();
        if len != self.prev_data.len() {
            return true;
        }
        if len < 4 {
            return frame.data != self.prev_data;
        }

        // Sample 64 evenly-spaced 4-byte pixels
        let step = (len / 256).max(4);
        let mut offset = 0;
        while offset + 4 <= len {
            if frame.data[offset..offset + 4] != self.prev_data[offset..offset + 4] {
                return true;
            }
            offset += step;
        }

        // Check last few bytes too
        if frame.data[len - 4..] != self.prev_data[len - 4..] {
            return true;
        }

        false
    }

    /// Full diff: return list of changed tiles. Updates internal state.
    pub fn diff(&mut self, frame: &Frame) -> Vec<DirtyTile> {
        let bpp = frame.format.bytes_per_pixel();
        let stride = frame.stride();
        let tiles_x = (frame.width + TILE_SIZE - 1) / TILE_SIZE;
        let tiles_y = (frame.height + TILE_SIZE - 1) / TILE_SIZE;

        let resolution_changed =
            !self.initialized || self.width != frame.width || self.height != frame.height;

        let mut dirty = Vec::new();

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let tile_w = (frame.width - tx * TILE_SIZE).min(TILE_SIZE);
                let tile_h = (frame.height - ty * TILE_SIZE).min(TILE_SIZE);
                let tile_x_px = (tx * TILE_SIZE) as usize;
                let tile_y_px = (ty * TILE_SIZE) as usize;

                let changed = if resolution_changed {
                    true
                } else {
                    tile_changed(
                        &frame.data,
                        &self.prev_data,
                        stride,
                        bpp,
                        tile_x_px,
                        tile_y_px,
                        tile_w as usize,
                        tile_h as usize,
                    )
                };

                if changed {
                    let tile_data = extract_tile(
                        &frame.data,
                        stride,
                        bpp,
                        tile_x_px,
                        tile_y_px,
                        tile_w as usize,
                        tile_h as usize,
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

        // Reuse allocation instead of clone
        self.prev_data.clear();
        self.prev_data.extend_from_slice(&frame.data);
        self.width = frame.width;
        self.height = frame.height;
        self.initialized = true;

        dirty
    }

    pub fn reset(&mut self) {
        self.initialized = false;
        self.prev_data.clear();
    }
}

fn tile_changed(
    curr: &[u8],
    prev: &[u8],
    stride: usize,
    bpp: usize,
    x_px: usize,
    y_px: usize,
    w: usize,
    h: usize,
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
    data: &[u8],
    stride: usize,
    bpp: usize,
    x_px: usize,
    y_px: usize,
    w: usize,
    h: usize,
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
        Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            data: vec![fill; (width * height * 4) as usize],
            timestamp: Instant::now(),
        }
    }

    #[test]
    fn first_frame_all_dirty() {
        let mut differ = TileDiffer::new();
        let frame = make_frame(128, 128, 0);
        let dirty = differ.diff(&frame);
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
    fn has_changes_fast_check() {
        let mut differ = TileDiffer::new();
        let frame = make_frame(128, 128, 42);
        assert!(differ.has_changes(&frame)); // first time
        differ.diff(&frame);
        assert!(!differ.has_changes(&frame)); // same frame

        let mut frame2 = make_frame(128, 128, 42);
        // Change the last 4 bytes (always checked by has_changes)
        let len = frame2.data.len();
        frame2.data[len - 1] = 99;
        assert!(differ.has_changes(&frame2)); // changed
    }

    #[test]
    fn one_pixel_change_one_dirty_tile() {
        let mut differ = TileDiffer::new();
        let frame1 = make_frame(128, 128, 0);
        differ.diff(&frame1);

        let mut frame2 = make_frame(128, 128, 0);
        let offset = 64 * 4; // pixel (64, 0) → tile (1, 0)
        frame2.data[offset] = 255;

        let dirty = differ.diff(&frame2);
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].tile_x, 1);
        assert_eq!(dirty[0].tile_y, 0);
    }

    #[test]
    fn non_aligned_resolution() {
        let mut differ = TileDiffer::new();
        let frame = make_frame(100, 100, 0);
        let dirty = differ.diff(&frame);
        assert_eq!(dirty.len(), 4);

        let bottom_right = dirty.iter().find(|t| t.tile_x == 1 && t.tile_y == 1).unwrap();
        assert_eq!(bottom_right.pixel_width, 36);
        assert_eq!(bottom_right.pixel_height, 36);
    }

    #[test]
    fn tile_data_correctness() {
        let mut differ = TileDiffer::new();
        let mut frame = make_frame(128, 64, 0);
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
