//! Phantom-local SCTP subset for WebRTC DataChannel.
//!
//! This crate is currently a repo-local derivative of `sctp-proto`, trimmed to
//! the API surface Phantom's browser WebRTC path still needs.

#![no_std]
#![warn(rust_2018_idioms)]
#![deny(clippy::std_instead_of_core)]
#![deny(clippy::std_instead_of_alloc)]
#![allow(dead_code)]
#![allow(clippy::bool_to_int_with_if)]
#![forbid(unsafe_code)]

#[macro_use]
extern crate alloc;

extern crate std;

use alloc::vec::Vec;
use bytes::Bytes;
use core::fmt;
use core::net::{IpAddr, SocketAddr};
use core::ops;
use std::time::Instant;

mod association;
pub use crate::association::Association;
pub use crate::association::Event;
pub use crate::association::stream::{Stream, StreamEvent, StreamId};

pub(crate) mod chunk;
pub use crate::chunk::chunk_payload_data::PayloadProtocolIdentifier;

mod config;
pub use crate::config::{EndpointConfig, ServerConfig};

mod endpoint;
pub use crate::endpoint::{AssociationHandle, DatagramEvent, Endpoint};

mod error;
pub use crate::error::Error;

mod packet;

mod shared;
pub use crate::shared::EcnCodepoint;

pub(crate) mod param;
pub(crate) mod queue;

pub(crate) mod util;

/// Fuzz helpers. Not part of the public API.
#[cfg(feature = "_fuzz")]
pub mod _fuzz {
    use bytes::BytesMut;

    use crate::packet::Packet;
    use crate::util::generate_packet_checksum;

    /// Feed arbitrary bytes into packet unmarshal.
    ///
    /// Patches a valid CRC32C checksum so the fuzzer can
    /// reach the parsing logic beyond the checksum check.
    pub fn fuzz_packet_unmarshal(data: &[u8]) {
        if data.len() < 12 {
            return;
        }
        let mut buf = BytesMut::from(data);
        // Zero checksum field before computing
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0;
        buf[11] = 0;
        let raw = buf.freeze();
        let checksum = generate_packet_checksum(&raw);
        let mut buf = BytesMut::from(raw.as_ref());
        buf[8..12].copy_from_slice(&checksum.to_le_bytes());
        let raw = buf.freeze();
        let _ = Packet::unmarshal(&raw);
    }
}

/// Whether an endpoint was the initiator of an association
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub enum Side {
    /// The initiator of an association
    #[default]
    Client = 0,
    /// The acceptor of an association
    Server = 1,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Side::Client => "Client",
            Side::Server => "Server",
        };
        write!(f, "{}", s)
    }
}

impl Side {
    #[inline]
    /// Shorthand for `self == Side::Client`
    pub fn is_client(self) -> bool {
        self == Side::Client
    }

    #[inline]
    /// Shorthand for `self == Side::Server`
    pub fn is_server(self) -> bool {
        self == Side::Server
    }
}

impl ops::Not for Side {
    type Output = Side;
    fn not(self) -> Side {
        match self {
            Side::Client => Side::Server,
            Side::Server => Side::Client,
        }
    }
}

use crate::packet::PartialDecode;

/// Payload in Incoming/outgoing Transmit
#[derive(Debug)]
pub enum Payload {
    PartialDecode(PartialDecode),
    RawEncode(Vec<Bytes>),
}

/// Incoming/outgoing Transmit
#[derive(Debug)]
pub struct Transmit {
    /// Received/Sent time
    pub now: Instant,
    /// The socket this datagram should be sent to
    pub remote: SocketAddr,
    /// Explicit congestion notification bits to set on the packet
    pub ecn: Option<EcnCodepoint>,
    /// Optional local IP address for the datagram
    pub local_ip: Option<IpAddr>,
    /// Payload of the datagram
    pub payload: Payload,
}

#[cfg(test)]
mod test {
    use alloc::sync::Arc;

    use super::*;

    #[test]
    fn ensure_send_sync() {
        fn is_send_sync(_a: impl Send + Sync) {}

        let c = EndpointConfig::new();
        let e = Endpoint::new(Arc::new(c), None);
        is_send_sync(e);

        let a = Association::default();
        is_send_sync(a);
    }
}
