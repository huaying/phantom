use anyhow::{Context, Result};
use openh264::encoder::{Encoder, EncoderConfig, FrameType};
use openh264::formats::YUVBuffer;
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_core::frame::Frame;

/// Wrapper to implement RGB8Source for BGRA data.
struct BgraFrame<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl openh264::formats::RGBSource for BgraFrame<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let idx = (y * self.width + x) * 4;
        let b = self.data[idx] as f32;
        let g = self.data[idx + 1] as f32;
        let r = self.data[idx + 2] as f32;
        (r, g, b)
    }
}

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: f32, bitrate_kbps: u32) -> Result<Self> {
        let config = EncoderConfig::new()
            .max_frame_rate(fps)
            .set_bitrate_bps(bitrate_kbps * 1000)
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .enable_skip_frame(true);

        let api = openh264::OpenH264API::from_source();
        let encoder =
            Encoder::with_api_config(api, config).context("failed to create OpenH264 encoder")?;

        tracing::info!(
            width,
            height,
            fps,
            bitrate_kbps,
            "OpenH264 encoder initialized"
        );
        Ok(Self {
            encoder,
            width,
            height,
        })
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn encode_frame(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;

        let bgra = BgraFrame {
            data: &frame.data,
            width: w,
            height: h,
        };
        let yuv = YUVBuffer::from_rgb_source(bgra);

        let bitstream = self.encoder.encode(&yuv).context("H.264 encode failed")?;
        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();

        Ok(EncodedFrame {
            codec: VideoCodec::H264,
            data,
            is_keyframe,
        })
    }

    fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
}
