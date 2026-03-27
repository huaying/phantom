use crate::protocol::Message;
use anyhow::Result;

/// A bidirectional connection that can send/receive protocol messages.
pub trait Connection: Send {
    fn send_msg(&mut self, msg: &Message) -> Result<()>;
    fn recv_msg(&mut self) -> Result<Message>;
}

/// Send-only half of a split connection.
pub trait MessageSender: Send {
    fn send_msg(&mut self, msg: &Message) -> Result<()>;
}

/// Receive-only half of a split connection.
pub trait MessageReceiver: Send {
    fn recv_msg(&mut self) -> Result<Message>;
}

/// Server-side transport: listens and accepts connections.
pub trait ServerTransport: Send {
    fn accept(&mut self) -> Result<Box<dyn Connection>>;
}

/// Client-side transport: connects to a server.
pub trait ClientTransport: Send {
    fn connect(&mut self) -> Result<Box<dyn Connection>>;
}
