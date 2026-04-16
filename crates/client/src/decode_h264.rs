use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use phantom_core::color::yuv420_to_rgb32;
use phantom_core::encode::FrameDecoder;

/// H.264 decoder using OpenH264 (CPU).
/// Handles mid-stream resolution changes automatically — OpenH264 adapts
/// when it encounters new SPS/PPS with different dimensions.
pub struct OpenH264Decoder {
    decoder: Decoder,
    width: usize,
    height: usize,
}

impl OpenH264Decoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let decoder = Decoder::new().context("failed to create OpenH264 decoder")?;
        tracing::info!(width, height, "OpenH264 decoder initialized");
        Ok(Self {
            decoder,
            width: width as usize,
            height: height as usize,
        })
    }

    /// Current decoded resolution (may change after decode_frame if server changed resolution).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }
}

impl FrameDecoder for OpenH264Decoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        let maybe_yuv = self
            .decoder
            .decode(data)
            .map_err(|e| anyhow::anyhow!("H.264 decode error: {e}"))?;
        let yuv = match maybe_yuv {
            Some(y) => y,
            None => anyhow::bail!(
                "decoder returned no output frame (data={} bytes)",
                data.len()
            ),
        };

        // OpenH264 handles SPS/PPS resolution changes automatically.
        // Read actual dimensions from the decoded YUV frame.
        let (y_stride, uv_stride, _) = yuv.strides();
        let (actual_w, actual_h) = yuv.dimensions();

        // Detect resolution change
        if actual_w != self.width || actual_h != self.height {
            tracing::info!(
                old_w = self.width,
                old_h = self.height,
                new_w = actual_w,
                new_h = actual_h,
                "decoder: resolution changed"
            );
            self.width = actual_w;
            self.height = actual_h;
        }

        let rgb32 = yuv420_to_rgb32(
            yuv.y(),
            yuv.u(),
            yuv.v(),
            self.width,
            self.height,
            y_stride,
            uv_stride,
        );

        Ok(rgb32)
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }
}
