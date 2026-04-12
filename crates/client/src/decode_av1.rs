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

        let picture = self.decoder.get_picture().context("dav1d get_picture")?;

        let w = picture.width() as usize;
        let h = picture.height() as usize;

        // Convert YUV to 0RGB
        let mut rgb = vec![0u32; w * h];

        match picture.pixel_layout() {
            dav1d::PixelLayout::I420 => {
                let y_stride = picture.stride(dav1d::PlanarImageComponent::Y) as usize;
                let u_stride = picture.stride(dav1d::PlanarImageComponent::U) as usize;
                let v_stride = picture.stride(dav1d::PlanarImageComponent::V) as usize;
                let y_plane = picture.plane(dav1d::PlanarImageComponent::Y);
                let u_plane = picture.plane(dav1d::PlanarImageComponent::U);
                let v_plane = picture.plane(dav1d::PlanarImageComponent::V);

                for row in 0..h {
                    for col in 0..w {
                        let y = y_plane[row * y_stride + col] as f32;
                        let u = u_plane[(row / 2) * u_stride + col / 2] as f32 - 128.0;
                        let v = v_plane[(row / 2) * v_stride + col / 2] as f32 - 128.0;

                        let r = (y + 1.402 * v).clamp(0.0, 255.0) as u32;
                        let g = (y - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u32;
                        let b = (y + 1.772 * u).clamp(0.0, 255.0) as u32;

                        rgb[row * w + col] = (r << 16) | (g << 8) | b;
                    }
                }
            }
            _ => {
                anyhow::bail!("unsupported pixel layout: {:?}", picture.pixel_layout());
            }
        }

        Ok(rgb)
    }
}
