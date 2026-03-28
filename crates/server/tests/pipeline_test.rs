use phantom_core::color::{bgra_to_yuv420, yuv420_to_rgb32};
use phantom_core::decode::{DecodedTile, Decoder};
use phantom_core::encode::{EncodedFrame, EncodedTile, Encoder, TileEncoding, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::protocol::{self, Message};
use phantom_core::tile::{DirtyTile, TileDiffer};
use std::io::Cursor;
use std::time::Instant;

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
fn tile_pipeline_roundtrip() {
    let mut differ = TileDiffer::new();
    let mut encoder = ZstdEncoder(3);
    let mut decoder = ZstdDecoder;

    let frame = make_frame(128, 128, 0xAB);
    let dirty = differ.diff(&frame);
    assert_eq!(dirty.len(), 4);

    let encoded = encoder.encode_tiles(&dirty).unwrap();
    let msg = Message::TileUpdate { sequence: 1, tiles: encoded };

    let mut buf = Vec::new();
    protocol::write_message(&mut buf, &msg).unwrap();
    let mut cursor = Cursor::new(&buf);
    let msg_back = protocol::read_message(&mut cursor).unwrap();

    match msg_back {
        Message::TileUpdate { sequence, tiles } => {
            assert_eq!(sequence, 1);
            assert_eq!(tiles.len(), 4);
            for tile in &tiles {
                let decoded = decoder.decode_tile(tile).unwrap();
                assert!(decoded.data.iter().all(|&b| b == 0xAB));
            }
        }
        _ => panic!("expected TileUpdate"),
    }
}

#[test]
fn h264_encode_decode_roundtrip() {
    use openh264::decoder::Decoder;
    use openh264::encoder::Encoder;
    use openh264::formats::{YUVBuffer, YUVSource};

    struct BgraFrame<'a>(&'a [u8], usize, usize);
    impl openh264::formats::RGBSource for BgraFrame<'_> {
        fn dimensions(&self) -> (usize, usize) { (self.1, self.2) }
        fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
            let i = (y * self.1 + x) * 4;
            (self.0[i+2] as f32, self.0[i+1] as f32, self.0[i] as f32)
        }
    }

    let w = 128usize;
    let h = 128usize;

    let mut bgra = vec![0u8; w * h * 4];
    for i in 0..w * h {
        bgra[i * 4] = 0;
        bgra[i * 4 + 1] = 0;
        bgra[i * 4 + 2] = 255;
        bgra[i * 4 + 3] = 255;
    }

    let yuv = YUVBuffer::from_rgb_source(BgraFrame(&bgra, w, h));
    let mut encoder = Encoder::new().unwrap();
    let bitstream = encoder.encode(&yuv).unwrap();
    let h264_data = bitstream.to_vec();
    assert!(!h264_data.is_empty());

    let mut decoder = Decoder::new().unwrap();
    let decoded = decoder.decode(&h264_data).unwrap().unwrap();
    let (y_stride, uv_stride, _) = decoded.strides();
    let rgb32 = yuv420_to_rgb32(decoded.y(), decoded.u(), decoded.v(), w, h, y_stride, uv_stride);

    let sample = rgb32[w * h / 2 + w / 2];
    let r = (sample >> 16) & 0xFF;
    let g = (sample >> 8) & 0xFF;
    let b = sample & 0xFF;
    assert!(r > 200, "expected red, got r={r} g={g} b={b}");
    assert!(g < 50, "expected no green, got g={g}");
    assert!(b < 50, "expected no blue, got b={b}");
}

#[test]
fn h264_pframe_smaller_than_keyframe() {
    use openh264::encoder::Encoder;
    use openh264::formats::YUVBuffer;

    struct BgraFrame<'a>(&'a [u8], usize, usize);
    impl openh264::formats::RGBSource for BgraFrame<'_> {
        fn dimensions(&self) -> (usize, usize) { (self.1, self.2) }
        fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
            let i = (y * self.1 + x) * 4;
            (self.0[i+2] as f32, self.0[i+1] as f32, self.0[i] as f32)
        }
    }

    let w = 640usize;
    let h = 480usize;
    let bgra = vec![128u8; w * h * 4];
    let yuv = YUVBuffer::from_rgb_source(BgraFrame(&bgra, w, h));

    let mut encoder = Encoder::new().unwrap();

    let bs1 = encoder.encode(&yuv).unwrap();
    let data1 = bs1.to_vec();

    let bs2 = encoder.encode(&yuv).unwrap();
    let data2 = bs2.to_vec();

    let raw_size = w * h * 4;
    eprintln!("Keyframe: {} bytes ({:.0}x)", data1.len(), raw_size as f64 / data1.len() as f64);
    eprintln!("P-frame:  {} bytes ({:.0}x)", data2.len(), raw_size as f64 / data2.len() as f64);

    assert!(data1.len() > 0);
    assert!(data2.len() < data1.len(), "P-frame of same content should be smaller than keyframe");
}

#[test]
fn protocol_message_roundtrip() {
    let messages = vec![
        Message::Hello { width: 1920, height: 1080, format: PixelFormat::Bgra8 },
        Message::Ping,
        Message::Pong,
        Message::TileUpdate { sequence: 42, tiles: vec![] },
        Message::VideoFrame {
            sequence: 1,
            frame: EncodedFrame {
                codec: VideoCodec::H264,
                data: vec![0, 0, 0, 1, 67],
                is_keyframe: true,
            },
        },
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
    differ.diff(&frame1);
    assert_eq!(differ.diff(&frame1).len(), 0);

    let mut frame2 = make_frame(256, 256, 100);
    frame2.data[0] = 200;
    let dirty = differ.diff(&frame2);
    assert_eq!(dirty.len(), 1);
}

#[test]
fn compression_ratio_solid_color() {
    let mut encoder = ZstdEncoder(3);
    let tile = DirtyTile {
        tile_x: 0, tile_y: 0,
        pixel_width: 64, pixel_height: 64,
        data: vec![0x42; 64 * 64 * 4],
    };
    let encoded = encoder.encode_tiles(&[tile]).unwrap();
    let ratio = (64 * 64 * 4) as f64 / encoded[0].data.len() as f64;
    assert!(ratio > 100.0, "solid color should compress >100x, got {ratio:.1}x");
}
