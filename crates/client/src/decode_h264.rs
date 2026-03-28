use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use phantom_core::color::yuv420_to_rgb32;
use phantom_core::encode::FrameDecoder;

/// H.264 decoder using OpenH264 (CPU).
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
}

impl FrameDecoder for OpenH264Decoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        let yuv = self
            .decoder
            .decode(data)
            .context("H.264 decode failed")?
            .context("decoder returned no frame")?;

        let (y_stride, uv_stride, _) = yuv.strides();

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
}
