use anyhow::{Context, Result};
use phantom_core::protocol::{self, Message};
use phantom_core::transport::{Connection, MessageReceiver, MessageSender, ServerTransport};
use std::net::{TcpListener, TcpStream};

pub struct TcpServerTransport {
    listener: TcpListener,
}

impl TcpServerTransport {
    pub fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .with_context(|| format!("failed to bind {addr}"))?;
        tracing::info!(addr, "TCP server listening");
        Ok(Self { listener })
    }
}

impl TcpServerTransport {
    /// Accept a connection, returning the concrete TcpConnection (for split support).
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
    /// Split into independent sender/receiver for concurrent bidirectional use.
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
