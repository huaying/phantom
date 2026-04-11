use anyhow::{Context, Result};
use phantom_core::crypto::{EncryptedReader, EncryptedWriter};
use phantom_core::protocol::{self, Message};
use phantom_core::transport::{ClientTransport, Connection, MessageReceiver, MessageSender};
use std::net::{Shutdown, TcpStream};

/// A handle that can shutdown the underlying TCP stream from any thread.
/// Calling `shutdown()` unblocks any blocking `read_exact()` calls on the stream.
pub struct TcpShutdownHandle {
    stream: TcpStream,
}

impl TcpShutdownHandle {
    /// Shutdown both halves of the TCP connection.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub struct TcpClientTransport {
    addr: String,
}

impl TcpClientTransport {
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
        }
    }

    pub fn connect_tcp(&self) -> Result<TcpConnection> {
        let stream = TcpStream::connect(&self.addr)
            .with_context(|| format!("failed to connect to {}", self.addr))?;
        stream.set_nodelay(true)?;
        tracing::info!(addr = %self.addr, "connected to server");
        Ok(TcpConnection { stream })
    }
}

impl ClientTransport for TcpClientTransport {
    fn connect(&mut self) -> Result<Box<dyn Connection>> {
        Ok(Box::new(self.connect_tcp()?))
    }
}

pub struct TcpConnection {
    stream: TcpStream,
}

impl TcpConnection {
    /// Get a shutdown handle that can be used to close the connection from another thread.
    pub fn shutdown_handle(&self) -> Result<TcpShutdownHandle> {
        Ok(TcpShutdownHandle {
            stream: self
                .stream
                .try_clone()
                .context("clone TcpStream for shutdown handle")?,
        })
    }

    pub fn split(self) -> Result<(PlainSender, PlainReceiver)> {
        let read_stream = self.stream.try_clone().context("clone TcpStream")?;
        Ok((
            PlainSender {
                stream: self.stream,
            },
            PlainReceiver {
                stream: read_stream,
            },
        ))
    }

    pub fn split_encrypted(self, key: &[u8; 32]) -> Result<(EncSender, EncReceiver)> {
        let read_stream = self.stream.try_clone().context("clone TcpStream")?;
        Ok((
            EncSender {
                writer: EncryptedWriter::new(self.stream, key),
            },
            EncReceiver {
                reader: EncryptedReader::new(read_stream, key),
            },
        ))
    }
}

impl Connection for TcpConnection {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        protocol::write_message(&mut self.stream, msg)
    }
    fn recv_msg(&mut self) -> Result<Message> {
        protocol::read_message(&mut self.stream)
    }
}

pub struct PlainSender {
    stream: TcpStream,
}

impl MessageSender for PlainSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        protocol::write_message(&mut self.stream, msg)
    }
}

pub struct PlainReceiver {
    stream: TcpStream,
}

impl MessageReceiver for PlainReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        protocol::read_message(&mut self.stream)
    }
}

pub struct EncSender {
    writer: EncryptedWriter<TcpStream>,
}

impl MessageSender for EncSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
        self.writer.write_encrypted(&payload)
    }
}

pub struct EncReceiver {
    reader: EncryptedReader<TcpStream>,
}

impl MessageReceiver for EncReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        let payload = self.reader.read_decrypted()?;
        bincode::deserialize(&payload).context("deserialize")
    }
}
