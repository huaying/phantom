use anyhow::{Context, Result};
use phantom_core::protocol::{self, Message};
use phantom_core::transport::{ClientTransport, Connection, MessageReceiver, MessageSender};
use std::net::TcpStream;

pub struct TcpClientTransport {
    addr: String,
}

impl TcpClientTransport {
    pub fn new(addr: &str) -> Self {
        Self { addr: addr.to_string() }
    }

    /// Connect and return the concrete TcpConnection (for split support).
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
    pub fn split(self) -> Result<(TcpSender, TcpReceiver)> {
        let read_stream = self.stream.try_clone().context("failed to clone TcpStream")?;
        Ok((
            TcpSender { stream: self.stream },
            TcpReceiver { stream: read_stream },
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

pub struct TcpSender {
    stream: TcpStream,
}

impl MessageSender for TcpSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        protocol::write_message(&mut self.stream, msg)
    }
}

pub struct TcpReceiver {
    stream: TcpStream,
}

impl MessageReceiver for TcpReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        protocol::read_message(&mut self.stream)
    }
}
