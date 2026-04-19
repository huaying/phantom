use crate::frame::Frame;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Av1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedFrame {
    pub codec: VideoCodec,
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// Full-frame video encoder (H.264, AV1, etc.).
/// Maintains internal state (reference frames) across calls.
pub trait FrameEncoder: Send {
    fn encode_frame(&mut self, frame: &Frame) -> Result<EncodedFrame>;

    /// Force next frame to be a keyframe (e.g., on client reconnect).
    fn force_keyframe(&mut self);

    /// Dynamically update the target bitrate (kbps).
    /// Returns Ok(()) if the encoder supports it, Err if not.
    fn set_bitrate_kbps(&mut self, _kbps: u32) -> Result<()> {
        anyhow::bail!("runtime bitrate change not supported by this encoder")
    }

    /// Get the current target bitrate in kbps.
    fn bitrate_kbps(&self) -> u32 {
        0
    }
}

/// Full-frame video decoder.
pub trait FrameDecoder: Send {
    /// Decode a video frame, returning raw pixel data in 0RGB u32 format (for display).
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>>;

    /// Current decoded resolution. May change after decode_frame if the stream
    /// contains new SPS/PPS (server changed resolution).
    fn dimensions(&self) -> (u32, u32);
}
