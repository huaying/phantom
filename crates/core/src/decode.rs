use crate::encode::EncodedTile;
use anyhow::Result;

/// Decoded tile: raw pixel data ready for compositing.
pub struct DecodedTile {
    pub tile_x: u32,
    pub tile_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub data: Vec<u8>,
}

/// Trait for decoding encoded tiles back to raw pixels.
///
/// Implementations:
/// - `ZstdDecoder`: CPU lossless decompression
/// - Future: `NvdecDecoder` (GPU H.264), `SoftH264Decoder` (CPU H.264)
pub trait Decoder: Send {
    fn decode_tile(&mut self, tile: &EncodedTile) -> Result<DecodedTile>;
}
