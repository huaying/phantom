use anyhow::{Context, Result};
use bytes::Bytes;
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use quinn::{Endpoint, ServerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// QUIC server transport using quinn.
///
/// Uses two channels:
/// - **Reliable stream**: control messages, keyframes, audio, file transfer
/// - **Unreliable datagrams**: video P-frames (fragmented to fit MTU)
pub struct QuicServerTransport {
    rt: Arc<Runtime>,
    endpoint: Endpoint,
}

/// Datagram fragment header (8 bytes):
/// - frame_id:     u32 — monotonically increasing per video frame
/// - chunk_idx:    u16 — index of this chunk within the frame
/// - total_chunks: u16 — total number of chunks for this frame
const DATAGRAM_HEADER_SIZE: usize = 8;

impl QuicServerTransport {
    pub fn bind(addr: &str) -> Result<Self> {
        let rt = Runtime::new().context("create tokio runtime")?;
        let addr: SocketAddr = addr.parse().context("parse address")?;

        let (server_config, cert_der) = generate_self_signed_config()?;
        let endpoint = rt
            .block_on(async { Endpoint::server(server_config, addr) })
            .context("bind QUIC endpoint")?;

        let fingerprint = ring_fingerprint(&cert_der);
        tracing::info!(%addr, fingerprint, "QUIC server listening (self-signed cert)");

        Ok(Self {
            rt: Arc::new(rt),
            endpoint,
        })
    }

    /// Accept a connection, return (sender, receiver) on separate streams.
    pub fn accept(&self) -> Result<(QuicSender, QuicReceiver)> {
        let rt = self.rt.clone();
        let conn = rt.block_on(async {
            let incoming = self
                .endpoint
                .accept()
                .await
                .context("no incoming connection")?;
            incoming.await.context("QUIC handshake failed")
        })?;

        let peer = conn.remote_address();
        let datagram_supported = conn.max_datagram_size().is_some();
        tracing::info!(%peer, datagram_supported, "QUIC client connected");

        // Open a bidirectional stream for reliable messages
        let (send_stream, recv_stream) =
            rt.block_on(async { conn.open_bi().await.context("open bidirectional stream") })?;

        Ok((
            QuicSender {
                rt: rt.clone(),
                stream: send_stream,
                conn: conn.clone(),
                datagram_frame_id: 0,
            },
            QuicReceiver {
                rt: rt.clone(),
                stream: recv_stream,
                conn,
                reassembler: DatagramReassembler::new(),
            },
        ))
    }
}

pub struct QuicSender {
    rt: Arc<Runtime>,
    stream: quinn::SendStream,
    conn: quinn::Connection,
    datagram_frame_id: u32,
}

impl QuicSender {
    /// Send a serialized message via unreliable datagrams, fragmenting as needed.
    /// Returns Ok(true) if sent via datagram, Ok(false) if datagrams unavailable.
    fn try_send_datagram(&mut self, payload: &[u8]) -> Result<bool> {
        let max_size = match self.conn.max_datagram_size() {
            Some(s) if s > DATAGRAM_HEADER_SIZE => s,
            _ => return Ok(false), // Datagrams not supported or too small
        };

        let chunk_payload_size = max_size - DATAGRAM_HEADER_SIZE;
        let total_chunks = payload.len().div_ceil(chunk_payload_size);

        if total_chunks > u16::MAX as usize {
            // Frame too large even for fragmentation — fall back to reliable
            return Ok(false);
        }

        let frame_id = self.datagram_frame_id;
        self.datagram_frame_id = self.datagram_frame_id.wrapping_add(1);

        for (idx, chunk) in payload.chunks(chunk_payload_size).enumerate() {
            let mut buf = Vec::with_capacity(DATAGRAM_HEADER_SIZE + chunk.len());
            buf.extend_from_slice(&frame_id.to_be_bytes());
            buf.extend_from_slice(&(idx as u16).to_be_bytes());
            buf.extend_from_slice(&(total_chunks as u16).to_be_bytes());
            buf.extend_from_slice(chunk);

            match self.conn.send_datagram(Bytes::from(buf)) {
                Ok(()) => {}
                Err(quinn::SendDatagramError::Disabled) => return Ok(false),
                Err(quinn::SendDatagramError::TooLarge) => {
                    // MTU changed mid-send; drop this frame, next keyframe recovers
                    tracing::debug!(frame_id, idx, "datagram too large, dropping frame");
                    return Ok(true); // Partial send = frame will be dropped by receiver anyway
                }
                Err(e) => return Err(e.into()),
            }
        }

        tracing::trace!(
            frame_id,
            total_chunks,
            payload_bytes = payload.len(),
            "sent video frame via datagrams"
        );
        Ok(true)
    }
}

impl MessageSender for QuicSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;

        // Route P-frames (non-keyframe video) via unreliable datagrams
        let use_datagram = matches!(
            msg,
            Message::VideoFrame { frame, .. } if !frame.is_keyframe
        );

        if use_datagram && self.try_send_datagram(&payload)? {
            return Ok(()); // Sent via datagram
        }
        // Fall through to reliable stream if datagrams unavailable or not a P-frame

        // Reliable stream: length-prefixed bincode
        let len = payload.len() as u32;
        self.rt.block_on(async {
            self.stream.write_all(&len.to_be_bytes()).await?;
            self.stream.write_all(&payload).await?;
            Ok::<_, anyhow::Error>(())
        })
    }
}

pub struct QuicReceiver {
    rt: Arc<Runtime>,
    stream: quinn::RecvStream,
    conn: quinn::Connection,
    reassembler: DatagramReassembler,
}

impl MessageReceiver for QuicReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        self.rt.block_on(async {
            loop {
                tokio::select! {
                    // Try to receive a datagram (unreliable video frame)
                    datagram = self.conn.read_datagram() => {
                        match datagram {
                            Ok(data) => {
                                if let Some(payload) = self.reassembler.feed(&data) {
                                    match bincode::deserialize::<Message>(&payload) {
                                        Ok(msg) => return Ok(msg),
                                        Err(_) => continue, // Corrupted frame, skip
                                    }
                                }
                                // Incomplete frame, keep reading
                            }
                            Err(e) => {
                                return Err(anyhow::anyhow!("datagram read error: {e}"));
                            }
                        }
                    }
                    // Try to receive from reliable stream
                    result = read_stream_message(&mut self.stream) => {
                        return result;
                    }
                }
            }
        })
    }
}

/// Read a length-prefixed bincode message from a QUIC stream.
async fn read_stream_message(stream: &mut quinn::RecvStream) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("read message length")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        anyhow::bail!("message too large ({len} bytes)");
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .context("read message payload")?;
    bincode::deserialize(&payload).context("deserialize")
}

// ── Datagram reassembly ─────────────────────────────────────────────────────

/// Reassembles fragmented datagram frames.
///
/// Each datagram has an 8-byte header: `[frame_id:u32][chunk_idx:u16][total_chunks:u16]`.
/// When all chunks for a frame arrive, the complete payload is returned.
/// Incomplete frames are discarded when a newer frame_id arrives.
struct DatagramReassembler {
    /// Currently reassembling frame
    current_frame_id: Option<u32>,
    /// Chunks received so far (indexed by chunk_idx)
    chunks: Vec<Option<Vec<u8>>>,
    /// How many chunks we expect
    total_chunks: u16,
    /// How many chunks we've received
    received_count: u16,
}

impl DatagramReassembler {
    fn new() -> Self {
        Self {
            current_frame_id: None,
            chunks: Vec::new(),
            total_chunks: 0,
            received_count: 0,
        }
    }

    /// Feed a raw datagram. Returns Some(complete_payload) when a frame is fully reassembled.
    fn feed(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < DATAGRAM_HEADER_SIZE {
            return None; // Runt datagram
        }

        let frame_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let chunk_idx = u16::from_be_bytes([data[4], data[5]]) as usize;
        let total_chunks = u16::from_be_bytes([data[6], data[7]]);
        let chunk_data = &data[DATAGRAM_HEADER_SIZE..];

        // If this is a newer frame, discard the old incomplete one
        if self.current_frame_id != Some(frame_id) {
            if let Some(old_id) = self.current_frame_id {
                if self.received_count < self.total_chunks {
                    tracing::trace!(
                        old_frame = old_id,
                        new_frame = frame_id,
                        received = self.received_count,
                        expected = self.total_chunks,
                        "dropping incomplete datagram frame"
                    );
                }
            }
            self.current_frame_id = Some(frame_id);
            self.total_chunks = total_chunks;
            self.received_count = 0;
            self.chunks.clear();
            self.chunks.resize(total_chunks as usize, None);
        }

        // Validate chunk index
        if chunk_idx >= self.chunks.len() {
            return None;
        }

        // Store chunk (ignore duplicates)
        if self.chunks[chunk_idx].is_none() {
            self.chunks[chunk_idx] = Some(chunk_data.to_vec());
            self.received_count += 1;
        }

        // Check if complete
        if self.received_count == self.total_chunks {
            let mut payload = Vec::new();
            for chunk in &self.chunks {
                payload.extend_from_slice(chunk.as_ref().unwrap());
            }
            // Reset for next frame
            self.current_frame_id = None;
            self.chunks.clear();
            Some(payload)
        } else {
            None
        }
    }
}

// -- Self-signed certificate generation --

fn generate_self_signed_config() -> Result<(ServerConfig, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["phantom".to_string()])
        .context("generate self-signed cert")?;

    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| anyhow::anyhow!("invalid key: {e}"))?;

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("TLS config")?;
    tls_config.alpn_protocols = vec![b"phantom".to_vec()];

    // Enable datagrams
    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024)); // 2MB receive buffer
    transport.datagram_send_buffer_size(2 * 1024 * 1024); // 2MB send buffer

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .map_err(|e| anyhow::anyhow!("QUIC server config: {e}"))?,
    ));
    server_config.transport_config(Arc::new(transport));

    Ok((server_config, cert_der))
}

fn ring_fingerprint(cert_der: &[u8]) -> String {
    use std::fmt::Write;
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut s = String::new();
    for (i, b) in digest.as_ref().iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{:02X}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembler_single_chunk() {
        let mut r = DatagramReassembler::new();
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_be_bytes()); // frame_id
        data.extend_from_slice(&0u16.to_be_bytes()); // chunk_idx
        data.extend_from_slice(&1u16.to_be_bytes()); // total_chunks
        data.extend_from_slice(b"hello");
        assert_eq!(r.feed(&data), Some(b"hello".to_vec()));
    }

    #[test]
    fn reassembler_multi_chunk() {
        let mut r = DatagramReassembler::new();

        // Chunk 0 of 3
        let mut d0 = Vec::new();
        d0.extend_from_slice(&1u32.to_be_bytes());
        d0.extend_from_slice(&0u16.to_be_bytes());
        d0.extend_from_slice(&3u16.to_be_bytes());
        d0.extend_from_slice(b"aaa");
        assert_eq!(r.feed(&d0), None);

        // Chunk 2 of 3 (out of order)
        let mut d2 = Vec::new();
        d2.extend_from_slice(&1u32.to_be_bytes());
        d2.extend_from_slice(&2u16.to_be_bytes());
        d2.extend_from_slice(&3u16.to_be_bytes());
        d2.extend_from_slice(b"ccc");
        assert_eq!(r.feed(&d2), None);

        // Chunk 1 of 3 (completes the frame)
        let mut d1 = Vec::new();
        d1.extend_from_slice(&1u32.to_be_bytes());
        d1.extend_from_slice(&1u16.to_be_bytes());
        d1.extend_from_slice(&3u16.to_be_bytes());
        d1.extend_from_slice(b"bbb");
        assert_eq!(r.feed(&d1), Some(b"aaabbbccc".to_vec()));
    }

    #[test]
    fn reassembler_new_frame_discards_old() {
        let mut r = DatagramReassembler::new();

        // Partial frame 0
        let mut d0 = Vec::new();
        d0.extend_from_slice(&0u32.to_be_bytes());
        d0.extend_from_slice(&0u16.to_be_bytes());
        d0.extend_from_slice(&2u16.to_be_bytes());
        d0.extend_from_slice(b"old");
        assert_eq!(r.feed(&d0), None);

        // New frame 1 arrives (single chunk) — should discard frame 0
        let mut d1 = Vec::new();
        d1.extend_from_slice(&1u32.to_be_bytes());
        d1.extend_from_slice(&0u16.to_be_bytes());
        d1.extend_from_slice(&1u16.to_be_bytes());
        d1.extend_from_slice(b"new");
        assert_eq!(r.feed(&d1), Some(b"new".to_vec()));
    }

    #[test]
    fn reassembler_runt_datagram() {
        let mut r = DatagramReassembler::new();
        assert_eq!(r.feed(&[0, 1, 2]), None); // Too short
    }
}
