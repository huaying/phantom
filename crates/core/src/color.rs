/// BGRA → YUV420p conversion.
/// Returns (Y plane, U plane, V plane).
pub fn bgra_to_yuv420(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y_plane = vec![0u8; width * height];
    let uv_w = width / 2;
    let uv_h = height / 2;
    let mut u_plane = vec![0u8; uv_w * uv_h];
    let mut v_plane = vec![0u8; uv_w * uv_h];

    for row in 0..height {
        for col in 0..width {
            let idx = (row * width + col) * 4;
            let b = bgra[idx] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;

            // BT.601 full-range
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row * width + col] = y.clamp(0, 255) as u8;

            // Subsample U/V at 2x2 blocks (top-left pixel)
            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (row / 2) * uv_w + col / 2;
                u_plane[uv_idx] = u.clamp(0, 255) as u8;
                v_plane[uv_idx] = v.clamp(0, 255) as u8;
            }
        }
    }

    (y_plane, u_plane, v_plane)
}

/// YUV420p → 0RGB u32 buffer (for minifb display).
pub fn yuv420_to_rgb32(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
) -> Vec<u32> {
    let mut buffer = vec![0u32; width * height];

    for row in 0..height {
        for col in 0..width {
            let y_val = y[row * y_stride + col] as i32 - 16;
            let u_val = u[(row / 2) * uv_stride + col / 2] as i32 - 128;
            let v_val = v[(row / 2) * uv_stride + col / 2] as i32 - 128;

            // BT.601
            let r = ((298 * y_val + 409 * v_val + 128) >> 8).clamp(0, 255) as u32;
            let g = ((298 * y_val - 100 * u_val - 208 * v_val + 128) >> 8).clamp(0, 255) as u32;
            let b = ((298 * y_val + 516 * u_val + 128) >> 8).clamp(0, 255) as u32;

            buffer[row * width + col] = (r << 16) | (g << 8) | b;
        }
    }

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_solid_white() {
        // White in BGRA
        let w = 16;
        let h = 16;
        let bgra: Vec<u8> = (0..w * h)
            .flat_map(|_| [255u8, 255, 255, 255])
            .collect();

        let (y, u, v) = bgra_to_yuv420(&bgra, w, h);
        let rgb32 = yuv420_to_rgb32(&y, &u, &v, w, h, w, w / 2);

        // Should be close to white (0x00FFFFFF) — allow YUV rounding error
        for &px in &rgb32 {
            let r = (px >> 16) & 0xFF;
            let g = (px >> 8) & 0xFF;
            let b = px & 0xFF;
            assert!(r >= 250, "r={r}");
            assert!(g >= 250, "g={g}");
            assert!(b >= 250, "b={b}");
        }
    }

    #[test]
    fn roundtrip_solid_black() {
        let w = 16;
        let h = 16;
        let bgra: Vec<u8> = (0..w * h)
            .flat_map(|_| [0u8, 0, 0, 255])
            .collect();

        let (y, u, v) = bgra_to_yuv420(&bgra, w, h);
        let rgb32 = yuv420_to_rgb32(&y, &u, &v, w, h, w, w / 2);

        for &px in &rgb32 {
            let r = (px >> 16) & 0xFF;
            let g = (px >> 8) & 0xFF;
            let b = px & 0xFF;
            assert!(r <= 5, "r={r}");
            assert!(g <= 5, "g={g}");
            assert!(b <= 5, "b={b}");
        }
    }
}
