use anyhow::{Context, Result};
use phantom_core::crypto::{EncryptedReader, EncryptedWriter};
use phantom_core::protocol::{self, Message};
use phantom_core::transport::{Connection, MessageReceiver, MessageSender, ServerTransport};
use std::net::{TcpListener, TcpStream};

pub struct TcpServerTransport {
    listener: TcpListener,
}

impl TcpServerTransport {
    pub fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
        tracing::info!(addr, "TCP server listening");
        Ok(Self { listener })
    }

    pub fn accept_tcp(&self) -> Result<TcpConnection> {
        let (stream, addr) = self.listener.accept().context("accept failed")?;
        stream.set_nodelay(true)?;
        tracing::info!(%addr, "client connected");
        Ok(TcpConnection { stream })
    }
}

impl ServerTransport for TcpServerTransport {
    fn accept(&mut self) -> Result<Box<dyn Connection>> {
        Ok(Box::new(self.accept_tcp()?))
    }
}

pub struct TcpConnection {
    stream: TcpStream,
}

impl TcpConnection {
    /// Split into plaintext sender/receiver.
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

    /// Split into encrypted sender/receiver.
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

// -- Plaintext sender/receiver --

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

// -- Encrypted sender/receiver --

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
