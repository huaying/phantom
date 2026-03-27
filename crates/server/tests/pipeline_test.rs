use phantom_core::decode::{DecodedTile, Decoder};
use phantom_core::encode::{EncodedTile, Encoder, TileEncoding};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::protocol::{self, Message};
use phantom_core::tile::{DirtyTile, TileDiffer};
use std::io::Cursor;
use std::time::Instant;

// Reuse server's encoder and a minimal decoder for testing

struct ZstdEncoder(i32);
impl Encoder for ZstdEncoder {
    fn encode_tiles(&mut self, tiles: &[DirtyTile]) -> anyhow::Result<Vec<EncodedTile>> {
        let mut out = Vec::with_capacity(tiles.len());
        for tile in tiles {
            let compressed = zstd::encode_all(tile.data.as_slice(), self.0)?;
            let (data, encoding) = if compressed.len() < tile.data.len() {
                (compressed, TileEncoding::Zstd)
            } else {
                (tile.data.clone(), TileEncoding::Raw)
            };
            out.push(EncodedTile {
                tile_x: tile.tile_x,
                tile_y: tile.tile_y,
                pixel_width: tile.pixel_width,
                pixel_height: tile.pixel_height,
                encoding,
                data,
            });
        }
        Ok(out)
    }
}

struct ZstdDecoder;
impl Decoder for ZstdDecoder {
    fn decode_tile(&mut self, tile: &EncodedTile) -> anyhow::Result<DecodedTile> {
        let data = match tile.encoding {
            TileEncoding::Zstd => zstd::decode_all(tile.data.as_slice())?,
            TileEncoding::Raw => tile.data.clone(),
            _ => panic!("unsupported encoding"),
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

fn make_frame(width: u32, height: u32, fill: u8) -> Frame {
    Frame {
        width,
        height,
        format: PixelFormat::Bgra8,
        data: vec![fill; (width * height * 4) as usize],
        timestamp: Instant::now(),
    }
}

#[test]
fn full_pipeline_roundtrip() {
    let mut differ = TileDiffer::new();
    let mut encoder = ZstdEncoder(3);
    let mut decoder = ZstdDecoder;

    let frame = make_frame(128, 128, 0xAB);
    let dirty = differ.diff(&frame);
    assert_eq!(dirty.len(), 4);

    let encoded = encoder.encode_tiles(&dirty).unwrap();
    assert_eq!(encoded.len(), 4);

    // Serialize to wire format and back
    let msg = Message::FrameUpdate { sequence: 1, tiles: encoded };
    let mut buf = Vec::new();
    protocol::write_message(&mut buf, &msg).unwrap();

    let mut cursor = Cursor::new(&buf);
    let msg_back = protocol::read_message(&mut cursor).unwrap();

    match msg_back {
        Message::FrameUpdate { sequence, tiles } => {
            assert_eq!(sequence, 1);
            assert_eq!(tiles.len(), 4);
            for tile in &tiles {
                let decoded = decoder.decode_tile(tile).unwrap();
                let expected_size = decoded.pixel_width as usize * decoded.pixel_height as usize * 4;
                assert_eq!(decoded.data.len(), expected_size);
                assert!(decoded.data.iter().all(|&b| b == 0xAB));
            }
        }
        _ => panic!("expected FrameUpdate"),
    }
}

#[test]
fn encode_decode_preserves_gradient() {
    let mut encoder = ZstdEncoder(3);
    let mut decoder = ZstdDecoder;

    let w = 64u32;
    let h = 64u32;
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.push((x & 0xFF) as u8);
            data.push((y & 0xFF) as u8);
            data.push(((x + y) & 0xFF) as u8);
            data.push(0xFF);
        }
    }

    let tiles = vec![DirtyTile {
        tile_x: 0, tile_y: 0,
        pixel_width: w, pixel_height: h,
        data: data.clone(),
    }];

    let encoded = encoder.encode_tiles(&tiles).unwrap();
    let decoded = decoder.decode_tile(&encoded[0]).unwrap();
    assert_eq!(decoded.data, data);
}

#[test]
fn protocol_message_roundtrip() {
    let messages = vec![
        Message::Hello { width: 1920, height: 1080, format: PixelFormat::Bgra8 },
        Message::Ping,
        Message::Pong,
        Message::FrameUpdate { sequence: 42, tiles: vec![] },
    ];

    for msg in &messages {
        let mut buf = Vec::new();
        protocol::write_message(&mut buf, msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = protocol::read_message(&mut cursor).unwrap();
        let mut buf2 = Vec::new();
        protocol::write_message(&mut buf2, &decoded).unwrap();
        assert_eq!(buf, buf2);
    }
}

#[test]
fn diff_detects_single_pixel_change() {
    let mut differ = TileDiffer::new();

    let frame1 = make_frame(256, 256, 100);
    differ.diff(&frame1); // first frame, all dirty

    let dirty = differ.diff(&frame1); // same → nothing
    assert_eq!(dirty.len(), 0);

    let mut frame2 = make_frame(256, 256, 100);
    frame2.data[0] = 200; // change one pixel in tile (0,0)
    let dirty = differ.diff(&frame2);
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].tile_x, 0);
    assert_eq!(dirty[0].tile_y, 0);
}

#[test]
fn compression_ratio_solid_color() {
    let mut encoder = ZstdEncoder(3);

    // Solid color should compress very well
    let tile = DirtyTile {
        tile_x: 0, tile_y: 0,
        pixel_width: 64, pixel_height: 64,
        data: vec![0x42; 64 * 64 * 4], // 16KB raw
    };

    let encoded = encoder.encode_tiles(&[tile]).unwrap();
    let ratio = (64 * 64 * 4) as f64 / encoded[0].data.len() as f64;
    assert!(ratio > 100.0, "solid color should compress >100x, got {ratio:.1}x");
    assert_eq!(encoded[0].encoding, TileEncoding::Zstd);
}
