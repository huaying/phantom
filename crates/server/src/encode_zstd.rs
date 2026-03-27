use anyhow::Result;
use phantom_core::encode::{EncodedTile, Encoder, TileEncoding};
use phantom_core::tile::DirtyTile;

/// CPU-based lossless encoder using zstd compression.
/// Good baseline: ~3-10x compression on typical screen content.
pub struct ZstdEncoder {
    /// Zstd compression level (1-22, default 3).
    level: i32,
}

impl ZstdEncoder {
    pub fn new(level: i32) -> Self {
        Self { level }
    }
}

impl Encoder for ZstdEncoder {
    fn encode_tiles(&mut self, tiles: &[DirtyTile]) -> Result<Vec<EncodedTile>> {
        let mut encoded = Vec::with_capacity(tiles.len());
        for tile in tiles {
            let compressed = zstd::encode_all(tile.data.as_slice(), self.level)?;

            // Only use compressed version if it's actually smaller.
            let (data, encoding) = if compressed.len() < tile.data.len() {
                (compressed, TileEncoding::Zstd)
            } else {
                (tile.data.clone(), TileEncoding::Raw)
            };

            encoded.push(EncodedTile {
                tile_x: tile.tile_x,
                tile_y: tile.tile_y,
                pixel_width: tile.pixel_width,
                pixel_height: tile.pixel_height,
                encoding,
                data,
            });
        }
        Ok(encoded)
    }
}
