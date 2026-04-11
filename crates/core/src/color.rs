//! BGRA ↔ YUV color space conversions with SIMD acceleration.
//!
//! All conversions use BT.601 full-range coefficients.
//! SIMD paths (AVX2, SSE4.1) are auto-detected at runtime on x86_64.
//! Fallback scalar path works on all platforms.

// ── BGRA → YUV420p ─────────────────────────────────────────────────────────

/// BGRA → YUV420p conversion.
/// Returns (Y plane, U plane, V plane).
pub fn bgra_to_yuv420(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y_plane = vec![0u8; width * height];
    let uv_w = width / 2;
    let uv_h = height / 2;
    let mut u_plane = vec![0u8; uv_w * uv_h];
    let mut v_plane = vec![0u8; uv_w * uv_h];

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                avx2::bgra_to_yuv420_avx2(
                    bgra,
                    width,
                    height,
                    &mut y_plane,
                    &mut u_plane,
                    &mut v_plane,
                );
            }
            return (y_plane, u_plane, v_plane);
        }
    }

    bgra_to_yuv420_scalar(
        bgra,
        width,
        height,
        &mut y_plane,
        &mut u_plane,
        &mut v_plane,
    );
    (y_plane, u_plane, v_plane)
}

/// Scalar fallback for BGRA → YUV420p.
#[allow(clippy::too_many_arguments)]
fn bgra_to_yuv420_scalar(
    bgra: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let uv_w = width / 2;

    for row in 0..height {
        let row_offset = row * width;
        for col in 0..width {
            let idx = (row_offset + col) * 4;
            let b = bgra[idx] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;

            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row_offset + col] = y.clamp(0, 255) as u8;

            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (row / 2) * uv_w + col / 2;
                u_plane[uv_idx] = u.clamp(0, 255) as u8;
                v_plane[uv_idx] = v.clamp(0, 255) as u8;
            }
        }
    }
}

// ── BGRA → NV12 ────────────────────────────────────────────────────────────

/// Convert BGRA to NV12 (BT.601) into a pre-allocated buffer.
/// NV12 layout: Y plane (w*h) followed by interleaved UV plane (w*h/2).
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe { avx2::bgra_to_nv12_avx2(bgra, width, height, nv12) };
            return;
        }
    }

    bgra_to_nv12_scalar(bgra, width, height, nv12);
}

/// Scalar fallback for BGRA → NV12.
fn bgra_to_nv12_scalar(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
    let (y_plane, uv_plane) = nv12.split_at_mut(width * height);

    for row in 0..height {
        let row_offset = row * width;
        for col in 0..width {
            let idx = (row_offset + col) * 4;
            let b = bgra[idx] as i32;
            let g = bgra[idx + 1] as i32;
            let r = bgra[idx + 2] as i32;

            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row_offset + col] = y.clamp(0, 255) as u8;

            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (row / 2) * width + col;
                uv_plane[uv_idx] = u.clamp(0, 255) as u8;
                uv_plane[uv_idx + 1] = v.clamp(0, 255) as u8;
            }
        }
    }
}

// ── YUV420p → 0RGB u32 ─────────────────────────────────────────────────────

/// YUV420p → 0RGB u32 buffer (for minifb/softbuffer display).
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

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                avx2::yuv420_to_rgb32_avx2(
                    y,
                    u,
                    v,
                    width,
                    height,
                    y_stride,
                    uv_stride,
                    &mut buffer,
                );
            }
            return buffer;
        }
    }

    yuv420_to_rgb32_scalar(y, u, v, width, height, y_stride, uv_stride, &mut buffer);
    buffer
}

/// Scalar fallback for YUV420p → 0RGB.
#[allow(clippy::too_many_arguments)]
fn yuv420_to_rgb32_scalar(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    buffer: &mut [u32],
) {
    for row in 0..height {
        for col in 0..width {
            let y_val = y[row * y_stride + col] as i32 - 16;
            let u_val = u[(row / 2) * uv_stride + col / 2] as i32 - 128;
            let v_val = v[(row / 2) * uv_stride + col / 2] as i32 - 128;

            let r = ((298 * y_val + 409 * v_val + 128) >> 8).clamp(0, 255) as u32;
            let g = ((298 * y_val - 100 * u_val - 208 * v_val + 128) >> 8).clamp(0, 255) as u32;
            let b = ((298 * y_val + 516 * u_val + 128) >> 8).clamp(0, 255) as u32;

            buffer[row * width + col] = (r << 16) | (g << 8) | b;
        }
    }
}

// ── AVX2 SIMD implementation ────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
mod avx2 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    /// Process 8 BGRA pixels → 8 Y values using AVX2.
    ///
    /// BT.601: Y = ((66*R + 129*G + 25*B + 128) >> 8) + 16
    ///
    /// We use 16-bit multiply-add in 256-bit registers to process
    /// 8 pixels per iteration.
    #[target_feature(enable = "avx2")]
    unsafe fn bgra8_to_y8(bgra: &[u8], y_out: &mut [u8]) {
        debug_assert!(bgra.len() >= 32 && y_out.len() >= 8);

        // Load 8 BGRA pixels = 32 bytes
        let src = _mm256_loadu_si256(bgra.as_ptr() as *const __m256i);

        // Shuffle to extract B, G, R into separate lanes.
        // Each BGRA pixel is [B, G, R, A] at offsets 0,1,2,3.
        // We need 8 B values, 8 G values, 8 R values as 16-bit.

        // Extract bytes: within each 128-bit lane, shuffle to group channels.
        // Lane 0: pixels 0-3, Lane 1: pixels 4-7
        // BGRA layout per pixel: b0 g0 r0 a0 | b1 g1 r1 a1 | b2 g2 r2 a2 | b3 g3 r3 a3

        // Shuffle mask to extract B bytes (indices 0,4,8,12 in each lane)
        let shuf_b = _mm256_setr_epi8(
            0, -1, 4, -1, 8, -1, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, -1, 4, -1, 8, -1, 12,
            -1, -1, -1, -1, -1, -1, -1, -1, -1,
        );
        let shuf_g = _mm256_setr_epi8(
            1, -1, 5, -1, 9, -1, 13, -1, -1, -1, -1, -1, -1, -1, -1, -1, 1, -1, 5, -1, 9, -1, 13,
            -1, -1, -1, -1, -1, -1, -1, -1, -1,
        );
        let shuf_r = _mm256_setr_epi8(
            2, -1, 6, -1, 10, -1, 14, -1, -1, -1, -1, -1, -1, -1, -1, -1, 2, -1, 6, -1, 10, -1, 14,
            -1, -1, -1, -1, -1, -1, -1, -1, -1,
        );

        // Shuffle: each lane gets 4 channel values as 16-bit in the low half
        let b_lo = _mm256_shuffle_epi8(src, shuf_b); // [b0,0,b1,0,b2,0,b3,0, 0..0 | b4,0,b5,0,b6,0,b7,0, 0..0]
        let g_lo = _mm256_shuffle_epi8(src, shuf_g);
        let r_lo = _mm256_shuffle_epi8(src, shuf_r);

        // Pack the two lanes together: permute so all 8 values are contiguous
        // Lane0 has pixels 0-3 in low 64 bits, Lane1 has pixels 4-7 in low 64 bits
        // Use permute4x64 to gather them: [lane0_lo64, lane1_lo64, ...]
        let b16 = _mm256_permute4x64_epi64(b_lo, 0b_10_00_10_00); // [p0-3, p4-7, p0-3, p4-7] — we only need low 128
        let g16 = _mm256_permute4x64_epi64(g_lo, 0b_10_00_10_00);
        let r16 = _mm256_permute4x64_epi64(r_lo, 0b_10_00_10_00);

        // Now low 128 bits of b16/g16/r16 contain 8x u16 values.
        // Use the low 128 bits for our 8 pixels.
        let b_128 = _mm256_castsi256_si128(b16);
        let g_128 = _mm256_castsi256_si128(g16);
        let r_128 = _mm256_castsi256_si128(r16);

        // Coefficients as 16-bit: 66, 129, 25
        let c_r = _mm_set1_epi16(66);
        let c_g = _mm_set1_epi16(129);
        let c_b = _mm_set1_epi16(25);
        let rnd = _mm_set1_epi16(128);
        let off = _mm_set1_epi16(16);

        // Y = ((66*R + 129*G + 25*B + 128) >> 8) + 16
        let yr = _mm_mullo_epi16(r_128, c_r);
        let yg = _mm_mullo_epi16(g_128, c_g);
        let yb = _mm_mullo_epi16(b_128, c_b);
        let y16 = _mm_add_epi16(
            _mm_srli_epi16(
                _mm_add_epi16(_mm_add_epi16(_mm_add_epi16(yr, yg), yb), rnd),
                8,
            ),
            off,
        );

        // Pack 16→8 with unsigned saturation
        let y8 = _mm_packus_epi16(y16, y16); // low 8 bytes are our Y values

        // Store 8 bytes
        // Use _mm_storel_epi64 to write 8 bytes
        _mm_storel_epi64(y_out.as_mut_ptr() as *mut __m128i, y8);
    }

    /// Compute U and V for a 2x2 block of BGRA pixels (top-left pixel only, BT.601).
    #[inline(always)]
    fn uv_from_bgra(bgra: &[u8], idx: usize) -> (u8, u8) {
        let b = bgra[idx] as i32;
        let g = bgra[idx + 1] as i32;
        let r = bgra[idx + 2] as i32;
        let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
        let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
        (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
    }

    /// BGRA → YUV420p with AVX2 Y-plane acceleration.
    ///
    /// # Safety
    /// Requires AVX2 support. Caller must verify with `is_x86_feature_detected!`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn bgra_to_yuv420_avx2(
        bgra: &[u8],
        width: usize,
        height: usize,
        y_plane: &mut [u8],
        u_plane: &mut [u8],
        v_plane: &mut [u8],
    ) {
        let uv_w = width / 2;

        for row in 0..height {
            let row_bgra = row * width * 4;
            let row_y = row * width;

            // Process Y in chunks of 8 pixels
            let mut col = 0;
            while col + 8 <= width {
                bgra8_to_y8(&bgra[row_bgra + col * 4..], &mut y_plane[row_y + col..]);
                col += 8;
            }
            // Scalar tail for remaining pixels
            while col < width {
                let idx = row_bgra + col * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                y_plane[row_y + col] = y.clamp(0, 255) as u8;
                col += 1;
            }

            // UV subsampling on even rows
            if row % 2 == 0 {
                let uv_row = row / 2;
                for c in (0..width).step_by(2) {
                    let idx = row_bgra + c * 4;
                    let (u, v) = uv_from_bgra(bgra, idx);
                    let uv_idx = uv_row * uv_w + c / 2;
                    u_plane[uv_idx] = u;
                    v_plane[uv_idx] = v;
                }
            }
        }
    }

    /// BGRA → NV12 with AVX2 Y-plane acceleration.
    ///
    /// # Safety
    /// Requires AVX2 support.
    #[target_feature(enable = "avx2")]
    pub unsafe fn bgra_to_nv12_avx2(bgra: &[u8], width: usize, height: usize, nv12: &mut [u8]) {
        let (y_plane, uv_plane) = nv12.split_at_mut(width * height);

        for row in 0..height {
            let row_bgra = row * width * 4;
            let row_y = row * width;

            // SIMD Y computation
            let mut col = 0;
            while col + 8 <= width {
                bgra8_to_y8(&bgra[row_bgra + col * 4..], &mut y_plane[row_y + col..]);
                col += 8;
            }
            while col < width {
                let idx = row_bgra + col * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                y_plane[row_y + col] = y.clamp(0, 255) as u8;
                col += 1;
            }

            // UV subsampling (interleaved NV12)
            if row % 2 == 0 {
                let uv_row = row / 2;
                for c in (0..width).step_by(2) {
                    let idx = row_bgra + c * 4;
                    let (u, v) = uv_from_bgra(bgra, idx);
                    let uv_idx = uv_row * width + c;
                    uv_plane[uv_idx] = u;
                    uv_plane[uv_idx + 1] = v;
                }
            }
        }
    }

    /// YUV420p → 0RGB u32 with AVX2 acceleration.
    ///
    /// Processes 8 pixels per iteration using 256-bit integer arithmetic.
    ///
    /// # Safety
    /// Requires AVX2 support.
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn yuv420_to_rgb32_avx2(
        y_data: &[u8],
        u_data: &[u8],
        v_data: &[u8],
        width: usize,
        height: usize,
        y_stride: usize,
        uv_stride: usize,
        buffer: &mut [u32],
    ) {
        // BT.601: R = ((298*(Y-16) + 409*(V-128) + 128) >> 8)
        //         G = ((298*(Y-16) - 100*(U-128) - 208*(V-128) + 128) >> 8)
        //         B = ((298*(Y-16) + 516*(U-128) + 128) >> 8)

        let c298 = _mm256_set1_epi32(298);
        let c409 = _mm256_set1_epi32(409);
        let c100 = _mm256_set1_epi32(100);
        let c208 = _mm256_set1_epi32(208);
        let c516 = _mm256_set1_epi32(516);
        let c128 = _mm256_set1_epi32(128);
        let zero = _mm256_setzero_si256();
        let max255 = _mm256_set1_epi32(255);

        for row in 0..height {
            let y_row = &y_data[row * y_stride..];
            let u_row = &u_data[(row / 2) * uv_stride..];
            let v_row = &v_data[(row / 2) * uv_stride..];
            let out_row = &mut buffer[row * width..];

            let mut col = 0;
            while col + 8 <= width {
                // Load 8 Y values, zero-extend to 32-bit
                let y_raw = _mm_loadl_epi64(y_row[col..].as_ptr() as *const __m128i);
                let y16 = _mm_unpacklo_epi8(y_raw, _mm_setzero_si128());
                let y32 = _mm256_cvtepu16_epi32(y16);
                let y_val = _mm256_sub_epi32(y32, _mm256_set1_epi32(16));

                // Load 4 U values (subsampled), duplicate each for 2 pixels
                let mut u_arr = [0i32; 8];
                let mut v_arr = [0i32; 8];
                for i in 0..4 {
                    let ui = u_row[col / 2 + i] as i32 - 128;
                    let vi = v_row[col / 2 + i] as i32 - 128;
                    u_arr[i * 2] = ui;
                    u_arr[i * 2 + 1] = ui;
                    v_arr[i * 2] = vi;
                    v_arr[i * 2 + 1] = vi;
                }
                let u_val = _mm256_loadu_si256(u_arr.as_ptr() as *const __m256i);
                let v_val = _mm256_loadu_si256(v_arr.as_ptr() as *const __m256i);

                // R = ((298*y + 409*v + 128) >> 8)
                let r32 = _mm256_srai_epi32(
                    _mm256_add_epi32(
                        _mm256_add_epi32(
                            _mm256_mullo_epi32(c298, y_val),
                            _mm256_mullo_epi32(c409, v_val),
                        ),
                        c128,
                    ),
                    8,
                );
                // G = ((298*y - 100*u - 208*v + 128) >> 8)
                let g32 = _mm256_srai_epi32(
                    _mm256_add_epi32(
                        _mm256_sub_epi32(
                            _mm256_sub_epi32(
                                _mm256_mullo_epi32(c298, y_val),
                                _mm256_mullo_epi32(c100, u_val),
                            ),
                            _mm256_mullo_epi32(c208, v_val),
                        ),
                        c128,
                    ),
                    8,
                );
                // B = ((298*y + 516*u + 128) >> 8)
                let b32 = _mm256_srai_epi32(
                    _mm256_add_epi32(
                        _mm256_add_epi32(
                            _mm256_mullo_epi32(c298, y_val),
                            _mm256_mullo_epi32(c516, u_val),
                        ),
                        c128,
                    ),
                    8,
                );

                // Clamp to [0, 255]
                let r_clamped = _mm256_min_epi32(_mm256_max_epi32(r32, zero), max255);
                let g_clamped = _mm256_min_epi32(_mm256_max_epi32(g32, zero), max255);
                let b_clamped = _mm256_min_epi32(_mm256_max_epi32(b32, zero), max255);

                // Pack: 0RGB = (R << 16) | (G << 8) | B
                let rgb = _mm256_or_si256(
                    _mm256_or_si256(
                        _mm256_slli_epi32(r_clamped, 16),
                        _mm256_slli_epi32(g_clamped, 8),
                    ),
                    b_clamped,
                );

                _mm256_storeu_si256(out_row[col..].as_mut_ptr() as *mut __m256i, rgb);
                col += 8;
            }

            // Scalar tail
            while col < width {
                let yv = y_row[col] as i32 - 16;
                let uv = u_row[col / 2] as i32 - 128;
                let vv = v_row[col / 2] as i32 - 128;
                let r = ((298 * yv + 409 * vv + 128) >> 8).clamp(0, 255) as u32;
                let g = ((298 * yv - 100 * uv - 208 * vv + 128) >> 8).clamp(0, 255) as u32;
                let b = ((298 * yv + 516 * uv + 128) >> 8).clamp(0, 255) as u32;
                out_row[col] = (r << 16) | (g << 8) | b;
                col += 1;
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_solid_white() {
        let w = 16;
        let h = 16;
        let bgra: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 255, 255, 255]).collect();

        let (y, u, v) = bgra_to_yuv420(&bgra, w, h);
        let rgb32 = yuv420_to_rgb32(&y, &u, &v, w, h, w, w / 2);

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
        let bgra: Vec<u8> = (0..w * h).flat_map(|_| [0u8, 0, 0, 255]).collect();

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

    #[test]
    fn simd_matches_scalar_yuv420() {
        let w = 1920;
        let h = 1080;
        // Generate a gradient pattern
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                bgra[idx] = ((x * 3 + y) % 256) as u8; // B
                bgra[idx + 1] = ((x + y * 7) % 256) as u8; // G
                bgra[idx + 2] = ((x * 5 + y * 3) % 256) as u8; // R
                bgra[idx + 3] = 255; // A
            }
        }

        // Scalar reference
        let mut y_scalar = vec![0u8; w * h];
        let mut u_scalar = vec![0u8; w / 2 * h / 2];
        let mut v_scalar = vec![0u8; w / 2 * h / 2];
        bgra_to_yuv420_scalar(&bgra, w, h, &mut y_scalar, &mut u_scalar, &mut v_scalar);

        // SIMD (via public API which auto-detects)
        let (y_simd, u_simd, v_simd) = bgra_to_yuv420(&bgra, w, h);

        assert_eq!(y_scalar, y_simd, "Y planes differ");
        assert_eq!(u_scalar, u_simd, "U planes differ");
        assert_eq!(v_scalar, v_simd, "V planes differ");
    }

    #[test]
    fn simd_matches_scalar_nv12() {
        let w = 1920;
        let h = 1080;
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                bgra[idx] = ((x * 3 + y) % 256) as u8;
                bgra[idx + 1] = ((x + y * 7) % 256) as u8;
                bgra[idx + 2] = ((x * 5 + y * 3) % 256) as u8;
                bgra[idx + 3] = 255;
            }
        }

        let mut nv12_scalar = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12_scalar(&bgra, w, h, &mut nv12_scalar);

        let mut nv12_simd = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12_simd);

        assert_eq!(nv12_scalar, nv12_simd, "NV12 buffers differ");
    }

    #[test]
    fn simd_matches_scalar_yuv_to_rgb() {
        let w = 1920;
        let h = 1080;
        // Create plausible YUV data
        let mut y_data = vec![0u8; w * h];
        let mut u_data = vec![0u8; w / 2 * h / 2];
        let mut v_data = vec![0u8; w / 2 * h / 2];
        for i in 0..y_data.len() {
            y_data[i] = (16 + (i % 220)) as u8;
        }
        for i in 0..u_data.len() {
            u_data[i] = (64 + (i % 128)) as u8;
        }
        for i in 0..v_data.len() {
            v_data[i] = (64 + (i % 128)) as u8;
        }

        let mut rgb_scalar = vec![0u32; w * h];
        yuv420_to_rgb32_scalar(&y_data, &u_data, &v_data, w, h, w, w / 2, &mut rgb_scalar);

        let rgb_simd = yuv420_to_rgb32(&y_data, &u_data, &v_data, w, h, w, w / 2);

        assert_eq!(rgb_scalar, rgb_simd, "RGB32 buffers differ");
    }

    #[test]
    fn nv12_solid_white() {
        let w = 16;
        let h = 16;
        let bgra = vec![255u8; w * h * 4];
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        let y_avg = nv12[..w * h].iter().map(|&v| v as u32).sum::<u32>() / (w * h) as u32;
        assert!(y_avg > 220 && y_avg < 240, "Y avg={y_avg}, expected ~235");

        let uv = &nv12[w * h..];
        let uv_avg = uv.iter().map(|&v| v as u32).sum::<u32>() / uv.len() as u32;
        assert!(
            uv_avg > 120 && uv_avg < 136,
            "UV avg={uv_avg}, expected ~128"
        );
    }

    #[test]
    fn nv12_solid_black() {
        let w = 16;
        let h = 16;
        let mut bgra = vec![0u8; w * h * 4];
        for i in 0..w * h {
            bgra[i * 4 + 3] = 255;
        }
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        let y_avg = nv12[..w * h].iter().map(|&v| v as u32).sum::<u32>() / (w * h) as u32;
        assert!(y_avg < 20, "Y avg={y_avg}, expected ~16");
    }

    #[test]
    fn nv12_correct_size() {
        let w = 1920;
        let h = 1080;
        let bgra = vec![128u8; w * h * 4];
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        assert_eq!(nv12.len(), w * h * 3 / 2);
    }

    /// Benchmark: scalar vs SIMD speed comparison (not a perf gate, just info).
    #[test]
    fn bench_comparison_1080p() {
        let w = 1920;
        let h = 1080;
        let bgra = vec![128u8; w * h * 4];
        let rounds = 10;

        // NV12
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            bgra_to_nv12_scalar(&bgra, w, h, &mut nv12);
        }
        let scalar_ms = start.elapsed().as_millis();

        let start = std::time::Instant::now();
        for _ in 0..rounds {
            bgra_to_nv12(&bgra, w, h, &mut nv12);
        }
        let simd_ms = start.elapsed().as_millis();

        let speedup = if simd_ms > 0 {
            scalar_ms as f64 / simd_ms as f64
        } else {
            f64::INFINITY
        };
        eprintln!(
            "BGRA→NV12 1080p ({rounds} rounds): scalar={scalar_ms}ms, simd={simd_ms}ms, speedup={speedup:.1}x"
        );

        // YUV→RGB
        let y = vec![128u8; w * h];
        let u = vec![128u8; w / 2 * h / 2];
        let v = vec![128u8; w / 2 * h / 2];
        let mut rgb = vec![0u32; w * h];

        let start = std::time::Instant::now();
        for _ in 0..rounds {
            yuv420_to_rgb32_scalar(&y, &u, &v, w, h, w, w / 2, &mut rgb);
        }
        let scalar_ms = start.elapsed().as_millis();

        let start = std::time::Instant::now();
        for _ in 0..rounds {
            let _ = yuv420_to_rgb32(&y, &u, &v, w, h, w, w / 2);
        }
        let simd_ms = start.elapsed().as_millis();

        let speedup = if simd_ms > 0 {
            scalar_ms as f64 / simd_ms as f64
        } else {
            f64::INFINITY
        };
        eprintln!(
            "YUV→RGB32 1080p ({rounds} rounds): scalar={scalar_ms}ms, simd={simd_ms}ms, speedup={speedup:.1}x"
        );
    }
}
