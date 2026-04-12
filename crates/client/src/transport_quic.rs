use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use quinn::Endpoint;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// QUIC client transport using quinn.
///
/// Uses two channels:
/// - **Reliable stream**: control messages, keyframes, audio, file transfer
/// - **Unreliable datagrams**: video P-frames (fragmented to fit MTU)
pub struct QuicClientTransport {
    rt: Arc<Runtime>,
}

/// Datagram fragment header (8 bytes):
/// - frame_id:     u32 — monotonically increasing per video frame
/// - chunk_idx:    u16 — index of this chunk within the frame
/// - total_chunks: u16 — total number of chunks for this frame
const DATAGRAM_HEADER_SIZE: usize = 8;

impl QuicClientTransport {
    pub fn new() -> Result<Self> {
        let rt = Runtime::new().context("create tokio runtime")?;
        Ok(Self { rt: Arc::new(rt) })
    }

    pub fn connect(&self, addr: &str) -> Result<(QuicSender, QuicReceiver)> {
        let server_addr: SocketAddr = addr.parse().context("parse server address")?;

        let conn = self.rt.block_on(async {
            let mut endpoint =
                Endpoint::client("0.0.0.0:0".parse().expect("hardcoded address is valid"))
                    .context("create client endpoint")?;

            let mut tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
                .with_no_client_auth();
            tls_config.alpn_protocols = vec![b"phantom".to_vec()];

            let mut client_config = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
                    .map_err(|e| anyhow::anyhow!("QUIC client config: {e}"))?,
            ));

            // Configure transport: keep-alive + datagram support
            let mut transport = quinn::TransportConfig::default();
            transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
            transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024)); // 2MB
            transport.datagram_send_buffer_size(2 * 1024 * 1024);
            client_config.transport_config(Arc::new(transport));

            endpoint.set_default_client_config(client_config);

            let conn = endpoint
                .connect(server_addr, "phantom")
                .context("initiate QUIC connection")?
                .await
                .context("QUIC handshake failed")?;

            let datagram_supported = conn.max_datagram_size().is_some();
            tracing::info!(
                peer = %conn.remote_address(),
                datagram_supported,
                "QUIC connected"
            );
            Ok::<_, anyhow::Error>(conn)
        })?;

        // Accept the bidirectional stream opened by the server
        let (send_stream, recv_stream) = self.rt.block_on(async {
            conn.accept_bi()
                .await
                .context("accept bidirectional stream from server")
        })?;

        Ok((
            QuicSender {
                rt: self.rt.clone(),
                stream: send_stream,
                conn: conn.clone(),
                datagram_frame_id: 0,
            },
            QuicReceiver {
                rt: self.rt.clone(),
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
    // conn and datagram_frame_id reserved for future client→server datagram support
    #[allow(dead_code)]
    conn: quinn::Connection,
    #[allow(dead_code)]
    datagram_frame_id: u32,
}

impl MessageSender for QuicSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;

        // Client→server: input events are small, always use reliable stream.
        // (Client doesn't send video, so no datagram routing needed here.)
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
                    // Try to receive a datagram (unreliable video P-frame)
                    datagram = self.conn.read_datagram() => {
                        match datagram {
                            Ok(data) => {
                                if let Some(payload) = self.reassembler.feed(&data) {
                                    match bincode::deserialize::<Message>(&payload) {
                                        Ok(msg) => return Ok(msg),
                                        Err(_) => continue, // Corrupted, skip
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

// ── Datagram reassembly (shared with server) ────────────────────────────────

struct DatagramReassembler {
    current_frame_id: Option<u32>,
    chunks: Vec<Option<Vec<u8>>>,
    total_chunks: u16,
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

    fn feed(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < DATAGRAM_HEADER_SIZE {
            return None;
        }

        let frame_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let chunk_idx = u16::from_be_bytes([data[4], data[5]]) as usize;
        let total_chunks = u16::from_be_bytes([data[6], data[7]]);
        let chunk_data = &data[DATAGRAM_HEADER_SIZE..];

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

        if chunk_idx >= self.chunks.len() {
            return None;
        }

        if self.chunks[chunk_idx].is_none() {
            self.chunks[chunk_idx] = Some(chunk_data.to_vec());
            self.received_count += 1;
        }

        if self.received_count == self.total_chunks {
            let mut payload = Vec::new();
            for chunk in &self.chunks {
                payload.extend_from_slice(chunk.as_ref().unwrap());
            }
            self.current_frame_id = None;
            self.chunks.clear();
            Some(payload)
        } else {
            None
        }
    }
}

// -- Skip server certificate verification --

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
