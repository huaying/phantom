use crate::tile::DirtyTile;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileEncoding {
    /// Uncompressed raw pixels.
    Raw,
    /// Zstd-compressed raw pixels (CPU, lossless).
    Zstd,
    /// H.264 lossy (future: NVENC / software x264).
    H264,
    /// AV1 lossy (future).
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

/// Trait for encoding dirty tiles into compressed payloads.
///
/// Implementations:
/// - `ZstdEncoder`: CPU lossless compression
/// - Future: `NvencEncoder` (GPU H.264), `SoftH264Encoder` (CPU H.264)
pub trait Encoder: Send {
    fn encode_tiles(&mut self, tiles: &[DirtyTile]) -> Result<Vec<EncodedTile>>;
}
