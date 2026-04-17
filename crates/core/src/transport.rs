use crate::protocol::Message;
use anyhow::Result;
use std::time::Duration;

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

    /// Best-effort timed read. Returns `Ok(None)` on timeout, `Ok(Some(msg))`
    /// on success, `Err` on transport-level failure. Default implementation
    /// is a plain blocking `recv_msg()` — the `timeout` hint is ignored; use
    /// this for transports that expose a native timeout (e.g. mpsc-backed).
    fn recv_msg_within(&mut self, _timeout: Duration) -> Result<Option<Message>> {
        self.recv_msg().map(Some)
    }
}

/// Server-side transport: listens and accepts connections.
pub trait ServerTransport: Send {
    fn accept(&mut self) -> Result<Box<dyn Connection>>;
}

/// Client-side transport: connects to a server.
pub trait ClientTransport: Send {
    fn connect(&mut self) -> Result<Box<dyn Connection>>;
}
