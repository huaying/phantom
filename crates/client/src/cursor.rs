// Local cursor overlay — drawn on the client framebuffer before present.
// Eliminates perceived mouse latency by rendering cursor at local position.
//
// 12x19 arrow cursor bitmap (1 = white, 2 = black outline, 0 = transparent)
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

const WHITE: u32 = 0x00FFFFFF;
const BLACK: u32 = 0x00000000;

/// Draw the local cursor onto a framebuffer at the given position.
/// Saves and restores the pixels underneath so the cursor can be "undrawn" next frame.
pub struct LocalCursor {
    /// Saved pixels under the cursor from last draw.
    saved_pixels: Vec<(usize, u32)>,
}

impl LocalCursor {
    pub fn new() -> Self {
        Self {
            saved_pixels: Vec::with_capacity(CURSOR_W * CURSOR_H),
        }
    }

    /// Restore pixels from the previous draw.
    pub fn undraw(&mut self, buffer: &mut [u32]) {
        for &(idx, pixel) in &self.saved_pixels {
            if idx < buffer.len() {
                buffer[idx] = pixel;
            }
        }
        self.saved_pixels.clear();
    }

    /// Draw cursor at (x, y) in framebuffer coordinates.
    pub fn draw(&mut self, buffer: &mut [u32], buf_width: usize, buf_height: usize, x: i32, y: i32) {
        self.saved_pixels.clear();

        for cy in 0..CURSOR_H {
            for cx in 0..CURSOR_W {
                let px = x as isize + cx as isize;
                let py = y as isize + cy as isize;

                if px < 0 || py < 0 || px >= buf_width as isize || py >= buf_height as isize {
                    continue;
                }

                let bitmap_val = CURSOR_BITMAP[cy * CURSOR_W + cx];
                if bitmap_val == 0 {
                    continue;
                }

                let idx = py as usize * buf_width + px as usize;
                self.saved_pixels.push((idx, buffer[idx]));

                buffer[idx] = match bitmap_val {
                    1 => WHITE,
                    2 => BLACK,
                    _ => continue,
                };
            }
        }
    }
}
