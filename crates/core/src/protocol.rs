use crate::encode::{EncodedFrame, EncodedTile};
use crate::frame::PixelFormat;
use crate::input::InputEvent;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum Message {
    /// Server → Client: initial handshake.
    Hello {
        width: u32,
        height: u32,
        format: PixelFormat,
    },

    /// Server → Client: H.264/AV1 encoded full frame (lossy, during motion).
    VideoFrame {
        sequence: u64,
        frame: EncodedFrame,
    },

    /// Server → Client: tile-based lossless update (quality refinement when static).
    TileUpdate {
        sequence: u64,
        tiles: Vec<EncodedTile>,
    },

    /// Client → Server: input event.
    Input(InputEvent),

    /// Bidirectional: clipboard content changed.
    ClipboardSync(String),

    Ping,
    Pong,
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
