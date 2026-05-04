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
    /// Cached Annex-B SPS/PPS prefix from a known-good keyframe.
    ///
    /// WebRTC clients can join right as Windows switches desktops, so every
    /// forced keyframe must be independently decodable.
    sps_pps: Vec<u8>,
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
            sps_pps: Vec::new(),
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
        let mut data = bitstream.to_vec();
        if is_keyframe {
            ensure_keyframe_has_sps_pps(&mut self.sps_pps, &mut data);
        }

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
        self.sps_pps.clear();
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

fn ensure_keyframe_has_sps_pps(cached_sps_pps: &mut Vec<u8>, data: &mut Vec<u8>) {
    let mut has_sps = false;
    let mut has_pps = false;
    let mut first_idr_start = None;

    for nal in annexb_nal_ranges(data) {
        let nal_type = data[nal.nal_start] & 0x1f;
        match nal_type {
            5 if first_idr_start.is_none() => first_idr_start = Some(nal.start_code_start),
            7 => has_sps = true,
            8 => has_pps = true,
            _ => {}
        }
    }

    if has_sps && has_pps {
        if cached_sps_pps.is_empty() {
            if let Some(idr_start) = first_idr_start {
                cached_sps_pps.extend_from_slice(&data[..idr_start]);
                tracing::debug!(
                    sps_pps_len = cached_sps_pps.len(),
                    "OpenH264: saved SPS/PPS from keyframe"
                );
            }
        }
    } else if !cached_sps_pps.is_empty() {
        let mut with_sps = Vec::with_capacity(cached_sps_pps.len() + data.len());
        with_sps.extend_from_slice(cached_sps_pps);
        with_sps.extend_from_slice(data);
        *data = with_sps;
    }
}

#[derive(Clone, Copy)]
struct AnnexBNalRange {
    start_code_start: usize,
    nal_start: usize,
}

fn annexb_nal_ranges(data: &[u8]) -> Vec<AnnexBNalRange> {
    let mut out = Vec::new();
    let mut search_from = 0usize;

    while let Some((start_code_start, start_code_len)) = find_annexb_start(data, search_from) {
        let nal_start = start_code_start + start_code_len;
        if nal_start < data.len() {
            out.push(AnnexBNalRange {
                start_code_start,
                nal_start,
            });
        }
        search_from = nal_start;
    }

    out
}

fn find_annexb_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 < data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            return Some((i, 4));
        }
        if data[i..].starts_with(&[0, 0, 1]) {
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_nal(data: &[u8], nal_type: u8) -> bool {
        annexb_nal_ranges(data)
            .into_iter()
            .any(|nal| (data[nal.nal_start] & 0x1f) == nal_type)
    }

    #[test]
    fn openh264_prepends_cached_sps_pps_to_forced_keyframe() {
        let mut cache = Vec::new();
        let mut first = vec![
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 0, 1, 0x68, 3, 4, // PPS
            0, 0, 0, 1, 0x65, 5, 6, // IDR
        ];
        ensure_keyframe_has_sps_pps(&mut cache, &mut first);
        assert_eq!(cache, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 4]);

        let mut forced = vec![0, 0, 0, 1, 0x65, 9, 10];
        ensure_keyframe_has_sps_pps(&mut cache, &mut forced);
        assert!(has_nal(&forced, 7));
        assert!(has_nal(&forced, 8));
        assert!(has_nal(&forced, 5));
    }

    #[test]
    fn openh264_sps_pps_cache_supports_three_byte_start_codes() {
        let mut cache = Vec::new();
        let mut first = vec![
            0, 0, 1, 0x67, 1, //
            0, 0, 1, 0x68, 2, //
            0, 0, 1, 0x65, 3,
        ];
        ensure_keyframe_has_sps_pps(&mut cache, &mut first);
        assert_eq!(cache, vec![0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2]);
    }
}
