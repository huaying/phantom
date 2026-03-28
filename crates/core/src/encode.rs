use crate::frame::Frame;
use crate::tile::DirtyTile;
use anyhow::Result;
use serde::{Deserialize, Serialize};

// -- Tile-level encoding (lossless, for quality updates) --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileEncoding {
    Raw,
    Zstd,
    H264,
    Av1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedTile {
    pub tile_x: u32,
    pub tile_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub encoding: TileEncoding,
    pub data: Vec<u8>,
}

pub trait Encoder: Send {
    fn encode_tiles(&mut self, tiles: &[DirtyTile]) -> Result<Vec<EncodedTile>>;
}

// -- Frame-level encoding (lossy, for motion) --

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
}

/// Full-frame video decoder.
pub trait FrameDecoder: Send {
    /// Decode a video frame, returning raw pixel data in 0RGB u32 format (for display).
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>>;
}
