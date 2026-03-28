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

/// Tile-level decoder (lossless quality updates).
pub trait Decoder: Send {
    fn decode_tile(&mut self, tile: &EncodedTile) -> Result<DecodedTile>;
}
