//! Integration tests for TCP transport and encryption.
//! Tests real TCP connections with the phantom protocol.

use std::thread;
use std::time::Duration;

use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::frame::PixelFormat;
use phantom_core::protocol::{self, Message};

/// Find a free port by binding to :0
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn tcp_hello_roundtrip() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();

    let addr_clone = addr.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let hello = Message::Hello {
            width: 1920,
            height: 1080,
            format: PixelFormat::Bgra8,
            protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
            audio: false,
            video_codec: phantom_core::encode::VideoCodec::H264,
        };
        protocol::write_message(&mut stream, &hello).unwrap();
        let reply = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(reply, Message::Pong));
    });

    thread::sleep(Duration::from_millis(50));

    let mut stream = std::net::TcpStream::connect(&addr_clone).unwrap();
    let msg = protocol::read_message(&mut stream).unwrap();
    match msg {
        Message::Hello { width, height, .. } => {
            assert_eq!(width, 1920);
            assert_eq!(height, 1080);
        }
        _ => panic!("expected Hello"),
    }
    protocol::write_message(&mut stream, &Message::Pong).unwrap();

    server.join().unwrap();
}

#[test]
fn tcp_video_frame_streaming() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();

    let addr_clone = addr.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for i in 0..10u64 {
            let frame = EncodedFrame {
                codec: VideoCodec::H264,
                data: vec![0, 0, 0, 1, 0x65, i as u8],
                is_keyframe: i == 0,
            };
            protocol::write_message(
                &mut stream,
                &Message::VideoFrame {
                    sequence: i,
                    frame: Box::new(frame),
                },
            )
            .unwrap();
        }
    });

    thread::sleep(Duration::from_millis(50));

    let mut stream = std::net::TcpStream::connect(&addr_clone).unwrap();
    for i in 0..10u64 {
        let msg = protocol::read_message(&mut stream).unwrap();
        match msg {
            Message::VideoFrame { sequence, frame } => {
                assert_eq!(sequence, i);
                assert_eq!(frame.is_keyframe, i == 0);
            }
            _ => panic!("expected VideoFrame at sequence {i}"),
        }
    }

    server.join().unwrap();
}

#[test]
fn tcp_bidirectional_input_and_video() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();

    let addr_clone = addr.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = stream;

        protocol::write_message(
            &mut writer,
            &Message::Hello {
                width: 1920,
                height: 1080,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
                video_codec: phantom_core::encode::VideoCodec::H264,
            },
        )
        .unwrap();

        let frame = EncodedFrame {
            codec: VideoCodec::H264,
            data: vec![0, 0, 0, 1, 0x65],
            is_keyframe: true,
        };
        protocol::write_message(
            &mut writer,
            &Message::VideoFrame {
                sequence: 0,
                frame: Box::new(frame),
            },
        )
        .unwrap();

        let msg = protocol::read_message(&mut reader).unwrap();
        assert!(matches!(msg, Message::Input(_)));

        let msg = protocol::read_message(&mut reader).unwrap();
        match msg {
            Message::ClipboardSync(text) => assert_eq!(text, "hello clipboard"),
            _ => panic!("expected ClipboardSync"),
        }
    });

    thread::sleep(Duration::from_millis(50));

    let stream = std::net::TcpStream::connect(&addr_clone).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = stream;

    assert!(matches!(
        protocol::read_message(&mut reader).unwrap(),
        Message::Hello { .. }
    ));
    assert!(matches!(
        protocol::read_message(&mut reader).unwrap(),
        Message::VideoFrame { .. }
    ));

    use phantom_core::input::{InputEvent, MouseButton};
    protocol::write_message(
        &mut writer,
        &Message::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }),
    )
    .unwrap();

    protocol::write_message(
        &mut writer,
        &Message::ClipboardSync("hello clipboard".to_string()),
    )
    .unwrap();

    server.join().unwrap();
}

#[test]
fn tcp_large_frame_64kb() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();
    let frame_size = 64 * 1024;

    let addr_clone = addr.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let frame = EncodedFrame {
            codec: VideoCodec::H264,
            data: vec![0xAB; frame_size],
            is_keyframe: true,
        };
        protocol::write_message(
            &mut stream,
            &Message::VideoFrame {
                sequence: 0,
                frame: Box::new(frame),
            },
        )
        .unwrap();
    });

    thread::sleep(Duration::from_millis(50));

    let mut stream = std::net::TcpStream::connect(&addr_clone).unwrap();
    match protocol::read_message(&mut stream).unwrap() {
        Message::VideoFrame { frame, .. } => {
            assert_eq!(frame.data.len(), frame_size);
            assert!(frame.data.iter().all(|&b| b == 0xAB));
        }
        _ => panic!("expected VideoFrame"),
    }

    server.join().unwrap();
}

#[test]
fn tcp_ping_pong_keepalive() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let listener = std::net::TcpListener::bind(&addr).unwrap();

    let addr_clone = addr.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for _ in 0..6 {
            protocol::write_message(&mut stream, &Message::Ping).unwrap();
            let msg = protocol::read_message(&mut stream).unwrap();
            assert!(matches!(msg, Message::Pong));
        }
    });

    thread::sleep(Duration::from_millis(50));

    let mut stream = std::net::TcpStream::connect(&addr_clone).unwrap();
    for _ in 0..6 {
        let msg = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(msg, Message::Ping));
        protocol::write_message(&mut stream, &Message::Pong).unwrap();
    }

    server.join().unwrap();
}

// ============================================================
// Encryption Tests (ChaCha20-Poly1305)
// ============================================================

#[cfg(feature = "crypto")]
mod crypto_tests {
    use super::*;
    use phantom_core::crypto::{EncryptedReader, EncryptedWriter};

    /// Helper: serialize message, encrypt, write
    fn send_encrypted(writer: &mut EncryptedWriter<std::net::TcpStream>, msg: &Message) {
        let payload = bincode::serialize(msg).unwrap();
        writer.write_encrypted(&payload).unwrap();
    }

    /// Helper: read encrypted, deserialize message
    fn recv_encrypted(
        reader: &mut EncryptedReader<std::net::TcpStream>,
    ) -> anyhow::Result<Message> {
        let payload = reader.read_decrypted()?;
        Ok(bincode::deserialize(&payload)?)
    }

    #[test]
    fn encrypted_hello_roundtrip() {
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let key = [0x42u8; 32];

        let listener = std::net::TcpListener::bind(&addr).unwrap();

        let addr_clone = addr.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = EncryptedWriter::new(stream.try_clone().unwrap(), &key);
            let mut reader = EncryptedReader::new(stream, &key);

            send_encrypted(
                &mut writer,
                &Message::Hello {
                    width: 3840,
                    height: 2160,
                    format: PixelFormat::Bgra8,
                    protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                    audio: false,
                    video_codec: phantom_core::encode::VideoCodec::H264,
                },
            );

            let reply = recv_encrypted(&mut reader).unwrap();
            assert!(matches!(reply, Message::Pong));
        });

        thread::sleep(Duration::from_millis(50));

        let stream = std::net::TcpStream::connect(&addr_clone).unwrap();
        let mut writer = EncryptedWriter::new(stream.try_clone().unwrap(), &key);
        let mut reader = EncryptedReader::new(stream, &key);

        match recv_encrypted(&mut reader).unwrap() {
            Message::Hello { width, height, .. } => {
                assert_eq!(width, 3840);
                assert_eq!(height, 2160);
            }
            _ => panic!("expected encrypted Hello"),
        }

        send_encrypted(&mut writer, &Message::Pong);
        server.join().unwrap();
    }

    #[test]
    fn wrong_key_rejected() {
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let server_key = [0x42u8; 32];
        let wrong_key = [0x00u8; 32];

        let listener = std::net::TcpListener::bind(&addr).unwrap();

        let addr_clone = addr.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = EncryptedWriter::new(stream, &server_key);
            send_encrypted(
                &mut writer,
                &Message::Hello {
                    width: 1920,
                    height: 1080,
                    format: PixelFormat::Bgra8,
                    protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                    audio: false,
                    video_codec: phantom_core::encode::VideoCodec::H264,
                },
            );
        });

        thread::sleep(Duration::from_millis(50));

        let stream = std::net::TcpStream::connect(&addr_clone).unwrap();
        let mut reader = EncryptedReader::new(stream, &wrong_key);

        let result = recv_encrypted(&mut reader);
        assert!(result.is_err(), "wrong key should fail decryption");

        server.join().unwrap();
    }

    #[test]
    fn encrypted_video_streaming() {
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let key = [0xDE; 32];

        let listener = std::net::TcpListener::bind(&addr).unwrap();

        let addr_clone = addr.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = EncryptedWriter::new(stream, &key);
            for i in 0..5u64 {
                let frame = EncodedFrame {
                    codec: VideoCodec::H264,
                    data: vec![0, 0, 0, 1, 0x65, i as u8],
                    is_keyframe: i == 0,
                };
                send_encrypted(
                    &mut writer,
                    &Message::VideoFrame {
                        sequence: i,
                        frame: Box::new(frame),
                    },
                );
            }
        });

        thread::sleep(Duration::from_millis(50));

        let stream = std::net::TcpStream::connect(&addr_clone).unwrap();
        let mut reader = EncryptedReader::new(stream, &key);

        for i in 0..5u64 {
            match recv_encrypted(&mut reader).unwrap() {
                Message::VideoFrame { sequence, frame } => {
                    assert_eq!(sequence, i);
                    assert_eq!(frame.is_keyframe, i == 0);
                }
                _ => panic!("expected encrypted VideoFrame"),
            }
        }

        server.join().unwrap();
    }
}
