/// Headless end-to-end test: mock server → TCP → client decoder pipeline.
/// No window, no screen capture. Tests the full network + codec path.
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::frame::PixelFormat;
use phantom_core::protocol::{self, Message};
use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

fn generate_test_bgra(width: usize, height: usize, frame_num: u32) -> Vec<u8> {
    let mut bgra = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            bgra[idx] = ((x + frame_num as usize * 3) % 256) as u8; // B
            bgra[idx + 1] = ((y + frame_num as usize * 7) % 256) as u8; // G
            bgra[idx + 2] = ((x + y) % 256) as u8; // R
            bgra[idx + 3] = 255;
        }
    }
    bgra
}

#[test]
fn headless_e2e_h264_over_tcp() {
    let width = 320u32;
    let height = 240u32;
    let num_frames = 10;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    // Server thread: encode and send H.264 frames
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let mut writer = BufWriter::new(stream);

        // Send Hello
        protocol::write_message(
            &mut writer,
            &Message::Hello {
                width,
                height,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
            },
        )
        .unwrap();

        // Encode and send frames
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::YUVBuffer;

        struct BgraFrame<'a>(&'a [u8], usize, usize);
        impl openh264::formats::RGBSource for BgraFrame<'_> {
            fn dimensions(&self) -> (usize, usize) {
                (self.1, self.2)
            }
            fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
                let i = (y * self.1 + x) * 4;
                (self.0[i + 2] as f32, self.0[i + 1] as f32, self.0[i] as f32)
            }
        }

        let config = EncoderConfig::new().set_bitrate_bps(500_000);
        let api = openh264::OpenH264API::from_source();
        let mut encoder = Encoder::with_api_config(api, config).unwrap();

        for i in 0..num_frames {
            let bgra = generate_test_bgra(width as usize, height as usize, i);
            let yuv = YUVBuffer::from_rgb_source(BgraFrame(&bgra, width as usize, height as usize));
            let bitstream = encoder.encode(&yuv).unwrap();
            let h264_data = bitstream.to_vec();

            protocol::write_message(
                &mut writer,
                &Message::VideoFrame {
                    sequence: i as u64 + 1,
                    frame: Box::new(EncodedFrame {
                        codec: VideoCodec::H264,
                        data: h264_data,
                        is_keyframe: i == 0,
                    }),
                },
            )
            .unwrap();
        }

        // Send a clipboard sync to test that path too
        protocol::write_message(
            &mut writer,
            &Message::ClipboardSync("test clipboard".to_string()),
        )
        .unwrap();

        thread::sleep(Duration::from_millis(100));
    });

    // Client thread: receive and decode
    let client = thread::spawn(move || {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_nodelay(true).unwrap();
        let mut reader = BufReader::new(stream);

        // Receive Hello
        let msg = protocol::read_message(&mut reader).unwrap();
        match msg {
            Message::Hello {
                width: w,
                height: h,
                ..
            } => {
                assert_eq!(w, width);
                assert_eq!(h, height);
            }
            _ => panic!("expected Hello"),
        }

        // Decode frames
        use openh264::decoder::Decoder;
        use openh264::formats::YUVSource;
        let mut decoder = Decoder::new().unwrap();
        let mut frames_decoded = 0;

        loop {
            let msg = match protocol::read_message(&mut reader) {
                Ok(m) => m,
                Err(_) => break,
            };

            match msg {
                Message::VideoFrame {
                    frame, sequence, ..
                } => {
                    let yuv = decoder.decode(&frame.data).unwrap();
                    if let Some(yuv) = yuv {
                        let (w, h) = yuv.dimensions();
                        assert_eq!(w, width as usize);
                        assert_eq!(h, height as usize);
                        // Verify we got actual pixel data
                        assert!(!yuv.y().is_empty());
                        frames_decoded += 1;
                    }
                    assert_eq!(sequence, frames_decoded);
                }
                Message::ClipboardSync(text) => {
                    assert_eq!(text, "test clipboard");
                }
                _ => {}
            }
        }

        assert_eq!(
            frames_decoded, num_frames as u64,
            "expected {num_frames} decoded frames, got {frames_decoded}"
        );
    });

    server.join().unwrap();
    client.join().unwrap();
}

#[test]
fn headless_e2e_encrypted_tcp() {
    use phantom_core::crypto::{EncryptedReader, EncryptedWriter};

    let width = 128u32;
    let height = 128u32;
    let key = [0x42u8; 32];

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = EncryptedWriter::new(stream, &key);

        let payload = bincode::serialize(&Message::Hello {
            width,
            height,
            format: PixelFormat::Bgra8,
            protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
            audio: false,
        })
        .unwrap();
        writer.write_encrypted(&payload).unwrap();

        let payload = bincode::serialize(&Message::ClipboardSync("encrypted!".into())).unwrap();
        writer.write_encrypted(&payload).unwrap();

        thread::sleep(Duration::from_millis(100));
    });

    let client = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let stream = TcpStream::connect(addr).unwrap();
        let mut reader = EncryptedReader::new(stream, &key);

        let payload = reader.read_decrypted().unwrap();
        let msg: Message = bincode::deserialize(&payload).unwrap();
        match msg {
            Message::Hello {
                width: w,
                height: h,
                ..
            } => {
                assert_eq!(w, width);
                assert_eq!(h, height);
            }
            _ => panic!("expected Hello"),
        }

        let payload = reader.read_decrypted().unwrap();
        let msg: Message = bincode::deserialize(&payload).unwrap();
        match msg {
            Message::ClipboardSync(text) => assert_eq!(text, "encrypted!"),
            _ => panic!("expected ClipboardSync"),
        }
    });

    server.join().unwrap();
    client.join().unwrap();
}
