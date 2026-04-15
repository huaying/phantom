use crate::encode::{EncodedFrame, EncodedTile};
use crate::frame::PixelFormat;
use crate::input::InputEvent;
use serde::{Deserialize, Serialize};

/// Current protocol version. Bump when adding/changing Message variants.
pub const PROTOCOL_VERSION: u32 = 5;

/// Minimum protocol version we can interoperate with.
/// Versions below this are rejected at handshake.
pub const MIN_PROTOCOL_VERSION: u32 = 2;

/// Audio codec identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
}

#[derive(Serialize, Deserialize)]
pub enum Message {
    /// Server → Client: initial handshake.
    Hello {
        width: u32,
        height: u32,
        format: PixelFormat,
        /// Protocol version (added in v2). Defaults to 1 if absent (bincode
        /// will deserialize old Hello payloads where this field is missing
        /// as the default value).
        #[serde(default = "default_protocol_version_1")]
        protocol_version: u32,
        /// Whether the server will send AudioFrame messages.
        #[serde(default)]
        audio: bool,
        /// Video codec used for VideoFrame messages (H.264 or AV1).
        #[serde(default = "default_video_codec")]
        video_codec: crate::encode::VideoCodec,
        /// Opaque session token for reconnect. Client sends this back in Resume.
        #[serde(default)]
        session_token: Vec<u8>,
    },

    /// Server → Client: H.264/AV1 encoded full frame (lossy, during motion).
    VideoFrame {
        sequence: u64,
        frame: Box<EncodedFrame>,
    },

    /// Server → Client: tile-based lossless update (quality refinement when static).
    TileUpdate {
        sequence: u64,
        tiles: Box<Vec<EncodedTile>>,
    },

    /// Client → Server: input event.
    Input(InputEvent),

    /// Bidirectional: clipboard content changed.
    ClipboardSync(String),

    /// Client → Server: paste this text into the focused app (type it out).
    PasteText(String),

    Ping,
    Pong,

    /// Server → Client: encoded audio chunk.
    AudioFrame {
        codec: AudioCodec,
        /// Sample rate in Hz (typically 48000 for Opus).
        sample_rate: u32,
        /// Number of channels (1 = mono, 2 = stereo).
        channels: u8,
        /// Encoded audio data (one Opus frame = 20ms).
        data: Vec<u8>,
    },

    /// Server → Client: graceful disconnect with reason.
    Disconnect {
        reason: String,
    },

    /// Client → Server: attempt to resume a previous session.
    /// If the token matches the active session, server skips Hello and
    /// sends a keyframe immediately. Otherwise server closes the connection.
    Resume {
        session_token: Vec<u8>,
        /// Last sequence number the client successfully decoded.
        last_sequence: u64,
    },

    /// Server → Client: resume accepted — session continues.
    ResumeOk,

    // ── File transfer (v3) ──────────────────────────────────────────────
    /// Bidirectional: offer to send a file.
    FileOffer {
        transfer_id: u64,
        name: String,
        size: u64,
    },

    /// Bidirectional: accept a file offer.
    FileAccept {
        transfer_id: u64,
    },

    /// Bidirectional: reject/cancel a file transfer.
    FileCancel {
        transfer_id: u64,
        reason: String,
    },

    /// Bidirectional: a chunk of file data.
    FileChunk {
        transfer_id: u64,
        offset: u64,
        data: Vec<u8>,
    },

    /// Bidirectional: file transfer complete (with SHA-256 for integrity).
    FileDone {
        transfer_id: u64,
        sha256: [u8; 32],
    },

    /// Client → Server: request display resolution change.
    /// Server adjusts the virtual display (VDD) to match client's viewport.
    /// Same approach as DCV/Sunshine — match server resolution to client window.
    ResolutionChange {
        width: u32,
        height: u32,
    },

    /// Server → Client: connection quality statistics (sent periodically).
    Stats {
        /// Server-measured round-trip time in microseconds (Ping→Pong EMA).
        rtt_us: u64,
        /// Server-side video frames per second.
        fps: f32,
        /// Server-side bandwidth in bytes per second.
        bandwidth_bps: u64,
        /// Server-side encode time per frame in microseconds (average).
        encode_us: u64,
    },
}

fn default_protocol_version_1() -> u32 {
    1
}

fn default_video_codec() -> crate::encode::VideoCodec {
    crate::encode::VideoCodec::H264
}

// -- Wire framing: [u32 big-endian length][bincode payload] --

use anyhow::{Context, Result};
use std::io::{Read, Write};

pub fn write_message(writer: &mut impl Write, msg: &Message) -> Result<()> {
    let payload = bincode::serialize(msg).context("serialize message")?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Maximum message size: 64 MB. Prevents OOM from malicious/corrupted length fields.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub fn read_message(reader: &mut impl Read) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("message too large ({len} bytes, max {MAX_MESSAGE_SIZE})");
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    let msg = bincode::deserialize(&payload).context("deserialize message")?;
    Ok(msg)
}

/// Read a message, returning `None` for unknown/undeserializable messages.
///
/// This is the forward-compatible variant: if a newer peer sends a message type
/// we don't recognize, we skip it rather than disconnecting.
pub fn read_message_lenient(reader: &mut impl Read) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("message too large ({len} bytes, max {MAX_MESSAGE_SIZE})");
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    match bincode::deserialize(&payload) {
        Ok(msg) => Ok(Some(msg)),
        Err(_) => Ok(None), // Unknown message variant — skip gracefully
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{EncodedFrame, VideoCodec};
    use crate::frame::PixelFormat;
    use std::io::Cursor;

    #[test]
    fn roundtrip_hello() {
        let msg = Message::Hello {
            width: 1920,
            height: 1080,
            format: PixelFormat::Bgra8,
            protocol_version: PROTOCOL_VERSION,
            audio: true,
            video_codec: VideoCodec::H264,
            session_token: vec![1, 2, 3, 4],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_message(&mut cursor).unwrap();
        match decoded {
            Message::Hello {
                width,
                height,
                protocol_version,
                audio,
                ..
            } => {
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert!(audio);
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn roundtrip_audio_frame() {
        let msg = Message::AudioFrame {
            codec: super::AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_message(&mut cursor).unwrap();
        match decoded {
            Message::AudioFrame {
                codec,
                sample_rate,
                channels,
                data,
            } => {
                assert_eq!(codec, super::AudioCodec::Opus);
                assert_eq!(sample_rate, 48000);
                assert_eq!(channels, 2);
                assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            }
            _ => panic!("expected AudioFrame"),
        }
    }

    #[test]
    fn roundtrip_video_frame() {
        let frame = EncodedFrame {
            codec: VideoCodec::H264,
            data: vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28], // fake SPS
            is_keyframe: true,
        };
        let msg = Message::VideoFrame {
            sequence: 42,
            frame: Box::new(frame),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_message(&mut cursor).unwrap();
        match decoded {
            Message::VideoFrame { sequence, frame } => {
                assert_eq!(sequence, 42);
                assert!(frame.is_keyframe);
                assert_eq!(frame.data.len(), 8);
            }
            _ => panic!("expected VideoFrame"),
        }
    }

    #[test]
    fn roundtrip_ping() {
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Ping).unwrap();
        let mut cursor = Cursor::new(&buf);
        assert!(matches!(read_message(&mut cursor).unwrap(), Message::Ping));
    }

    #[test]
    fn roundtrip_disconnect() {
        let msg = Message::Disconnect {
            reason: "replaced by new client".to_string(),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        match read_message(&mut cursor).unwrap() {
            Message::Disconnect { reason } => assert_eq!(reason, "replaced by new client"),
            _ => panic!("expected Disconnect"),
        }
    }

    #[test]
    fn roundtrip_file_offer() {
        let msg = Message::FileOffer {
            transfer_id: 42,
            name: "test.txt".to_string(),
            size: 1024,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        match read_message(&mut cursor).unwrap() {
            Message::FileOffer {
                transfer_id,
                name,
                size,
            } => {
                assert_eq!(transfer_id, 42);
                assert_eq!(name, "test.txt");
                assert_eq!(size, 1024);
            }
            _ => panic!("expected FileOffer"),
        }
    }

    #[test]
    fn roundtrip_file_chunk() {
        let msg = Message::FileChunk {
            transfer_id: 7,
            offset: 256000,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        match read_message(&mut cursor).unwrap() {
            Message::FileChunk {
                transfer_id,
                offset,
                data,
            } => {
                assert_eq!(transfer_id, 7);
                assert_eq!(offset, 256000);
                assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            }
            _ => panic!("expected FileChunk"),
        }
    }

    #[test]
    fn roundtrip_file_done() {
        let hash = [0xABu8; 32];
        let msg = Message::FileDone {
            transfer_id: 99,
            sha256: hash,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        match read_message(&mut cursor).unwrap() {
            Message::FileDone {
                transfer_id,
                sha256,
            } => {
                assert_eq!(transfer_id, 99);
                assert_eq!(sha256, hash);
            }
            _ => panic!("expected FileDone"),
        }
    }

    #[test]
    fn multiple_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Ping).unwrap();
        write_message(&mut buf, &Message::Pong).unwrap();
        write_message(&mut buf, &Message::ClipboardSync("hello".to_string())).unwrap();

        let mut cursor = Cursor::new(&buf);
        assert!(matches!(read_message(&mut cursor).unwrap(), Message::Ping));
        assert!(matches!(read_message(&mut cursor).unwrap(), Message::Pong));
        match read_message(&mut cursor).unwrap() {
            Message::ClipboardSync(text) => assert_eq!(text, "hello"),
            _ => panic!("expected ClipboardSync"),
        }
    }

    #[test]
    fn roundtrip_stats() {
        let msg = Message::Stats {
            rtt_us: 15_000,
            fps: 59.8,
            bandwidth_bps: 5_000_000,
            encode_us: 2_500,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        match read_message(&mut cursor).unwrap() {
            Message::Stats {
                rtt_us,
                fps,
                bandwidth_bps,
                encode_us,
            } => {
                assert_eq!(rtt_us, 15_000);
                assert!((fps - 59.8).abs() < 0.1);
                assert_eq!(bandwidth_bps, 5_000_000);
                assert_eq!(encode_us, 2_500);
            }
            _ => panic!("expected Stats"),
        }
    }

    #[test]
    fn lenient_reader_skips_unknown() {
        // Construct a framed payload with an invalid enum variant index.
        // bincode uses u32 for enum variant, so variant 0xFFFFFFFF should not exist.
        let bogus_variant: u32 = 0xFFFF_FFFF;
        let bogus_payload = bincode::serialize(&bogus_variant).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&(bogus_payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&bogus_payload);
        // Append a valid Ping message after it
        write_message(&mut buf, &Message::Ping).unwrap();

        let mut cursor = Cursor::new(&buf);
        // First message: unknown → should return None (not error)
        assert!(read_message_lenient(&mut cursor).unwrap().is_none());
        // Second message: valid Ping
        let msg = read_message_lenient(&mut cursor).unwrap();
        assert!(matches!(msg, Some(Message::Ping)));
    }
}
