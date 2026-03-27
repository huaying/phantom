use anyhow::{bail, Result};
use phantom_core::decode::{DecodedTile, Decoder};
use phantom_core::encode::{EncodedTile, TileEncoding};

/// CPU-based lossless decoder using zstd decompression.
pub struct ZstdDecoder;

impl ZstdDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl Decoder for ZstdDecoder {
    fn decode_tile(&mut self, tile: &EncodedTile) -> Result<DecodedTile> {
        let data = match tile.encoding {
            TileEncoding::Raw => tile.data.clone(),
            TileEncoding::Zstd => zstd::decode_all(tile.data.as_slice())?,
            other => bail!("unsupported encoding: {:?}", other),
        };

        Ok(DecodedTile {
            tile_x: tile.tile_x,
            tile_y: tile.tile_y,
            pixel_width: tile.pixel_width,
            pixel_height: tile.pixel_height,
            data,
        })
    }
}
