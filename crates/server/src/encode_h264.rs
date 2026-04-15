use anyhow::{Context, Result};
use openh264::encoder::{Encoder, EncoderConfig, FrameType};
use openh264::formats::YUVBuffer;
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_core::frame::Frame;

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    fps: f32,
    bitrate_kbps: u32,
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
            fps,
            bitrate_kbps,
        })
    }
}

impl FrameEncoder for OpenH264Encoder {
    fn encode_frame(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;

        // Use SIMD BGRA→YUV420 conversion (AVX2 on x86, scalar fallback).
        // ~2.8x faster than the old per-pixel f32 path (pixel_f32 callback).
        let (y, u, v) = phantom_core::color::bgra_to_yuv420(&frame.data, w, h);

        // Pack Y + U + V into a single contiguous buffer for OpenH264.
        let mut yuv_packed = Vec::with_capacity(y.len() + u.len() + v.len());
        yuv_packed.extend_from_slice(&y);
        yuv_packed.extend_from_slice(&u);
        yuv_packed.extend_from_slice(&v);

        let yuv = YUVBuffer::from_vec(yuv_packed, w, h);

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

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        if kbps == self.bitrate_kbps {
            return Ok(());
        }
        // OpenH264 doesn't support runtime reconfiguration,
        // so we recreate the encoder with the new bitrate.
        let fps = self.fps;
        let config = EncoderConfig::new()
            .max_frame_rate(fps)
            .set_bitrate_bps(kbps * 1000)
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
            .enable_skip_frame(true);
        let api = openh264::OpenH264API::from_source();
        self.encoder =
            Encoder::with_api_config(api, config).context("failed to recreate OpenH264 encoder")?;
        tracing::info!(
            old_kbps = self.bitrate_kbps,
            new_kbps = kbps,
            "OpenH264 bitrate reconfigured"
        );
        self.bitrate_kbps = kbps;
        Ok(())
    }

    fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }
}
