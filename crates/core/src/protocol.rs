use crate::encode::{EncodedFrame, EncodedTile};
use crate::frame::PixelFormat;
use crate::input::InputEvent;
use serde::{Deserialize, Serialize};

/// Current protocol version. Bump when adding/changing Message variants.
pub const PROTOCOL_VERSION: u32 = 2;

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
}

fn default_protocol_version_1() -> u32 {
    1
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
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_message(&mut cursor).unwrap();
        match decoded {
            Message::Hello { width, height, protocol_version, audio, .. } => {
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
            Message::AudioFrame { codec, sample_rate, channels, data } => {
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
        let msg = Message::VideoFrame { sequence: 42, frame: Box::new(frame) };
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
}
