//! AV1 decode via dav1d.

use anyhow::{Context, Result};
use phantom_core::encode::FrameDecoder;

pub struct Dav1dDecoder {
    decoder: dav1d::Decoder,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
}

impl Dav1dDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let settings = dav1d::Settings::new();
        let decoder =
            dav1d::Decoder::with_settings(&settings).context("failed to create dav1d decoder")?;
        tracing::info!(width, height, "dav1d AV1 decoder initialized");
        Ok(Self {
            decoder,
            width,
            height,
        })
    }
}

impl FrameDecoder for Dav1dDecoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        self.decoder
            .send_data(data.to_vec(), None, None, None)
            .context("dav1d send_data")?;

        // dav1d may return EAGAIN if the frame doesn't produce output yet
        let picture = match self.decoder.get_picture() {
            Ok(pic) => pic,
            Err(dav1d::Error::Again) => {
                // Decoder buffered the data but needs more to produce output.
                // Return empty — caller will skip this frame.
                return Ok(vec![]);
            }
            Err(e) => {
                anyhow::bail!("dav1d decode error: {e}");
            }
        };

        let w = picture.width() as usize;
        let h = picture.height() as usize;

        match picture.pixel_layout() {
            dav1d::PixelLayout::I420 => {
                let y_stride = picture.stride(dav1d::PlanarImageComponent::Y) as usize;
                let u_stride = picture.stride(dav1d::PlanarImageComponent::U) as usize;
                let y_plane = picture.plane(dav1d::PlanarImageComponent::Y);
                let u_plane = picture.plane(dav1d::PlanarImageComponent::U);
                let v_plane = picture.plane(dav1d::PlanarImageComponent::V);

                // Use SIMD-accelerated conversion from phantom_core::color
                Ok(phantom_core::color::yuv420_to_rgb32(
                    &y_plane, &u_plane, &v_plane, w, h, y_stride, u_stride,
                ))
            }
            _ => {
                anyhow::bail!("unsupported pixel layout: {:?}", picture.pixel_layout());
            }
        }
    }
}
