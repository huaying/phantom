use crate::encode::EncodedTile;
use crate::frame::PixelFormat;
use crate::input::InputEvent;
use serde::{Deserialize, Serialize};

/// Wire protocol messages between server and client.
#[derive(Serialize, Deserialize)]
pub enum Message {
    /// Server → Client: initial handshake with display info.
    Hello {
        width: u32,
        height: u32,
        format: PixelFormat,
    },

    /// Server → Client: a set of updated tiles for one frame.
    FrameUpdate {
        sequence: u64,
        tiles: Vec<EncodedTile>,
    },

    /// Client → Server: input event.
    Input(InputEvent),

    /// Either direction: keep-alive ping.
    Ping,

    /// Either direction: keep-alive pong.
    Pong,
}

// -- Wire framing helpers for length-prefixed bincode over TCP --

use anyhow::{Context, Result};
use std::io::{Read, Write};

/// Write a message as [u32 big-endian length][bincode payload].
pub fn write_message(writer: &mut impl Write, msg: &Message) -> Result<()> {
    let payload = bincode::serialize(msg).context("serialize message")?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Read a message from [u32 big-endian length][bincode payload].
pub fn read_message(reader: &mut impl Read) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    let msg = bincode::deserialize(&payload).context("deserialize message")?;
    Ok(msg)
}
