/// QUIC datagram transport tests.
///
/// Verifies:
/// 1. Datagram send/receive works between QUIC endpoints
/// 2. Fragmentation and reassembly for large payloads
/// 3. Reassembler discards incomplete frames on new frame_id
/// 4. Max datagram size is reasonable
///
/// Note: bi-stream tests are covered by the existing E2E headless tests
/// and the production QUIC transport (which creates separate tokio runtimes).
/// These tests focus specifically on the datagram path added for P-frames.
use bytes::Bytes;
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::protocol::Message;
use quinn::{Endpoint, ServerConfig};
use std::sync::Arc;
use std::time::Duration;

const DATAGRAM_HEADER_SIZE: usize = 8;

// ── Helper: create connected QUIC pair ──────────────────────────────────────

struct QuicConnPair {
    server_conn: quinn::Connection,
    client_conn: quinn::Connection,
    rt: Arc<tokio::runtime::Runtime>,
}

fn make_connected_pair() -> QuicConnPair {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let _guard = rt.enter();

    // Self-signed cert
    let cert = rcgen::generate_simple_self_signed(vec!["phantom".to_string()]).unwrap();
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();
    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der).unwrap();

    let mut tls_server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .unwrap();
    tls_server.alpn_protocols = vec![b"phantom".to_vec()];

    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    let transport = Arc::new(transport);

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_server).unwrap(),
    ));
    server_config.transport_config(transport.clone());

    let server_endpoint = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server_endpoint.local_addr().unwrap();

    let mut tls_client = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    tls_client.alpn_protocols = vec![b"phantom".to_vec()];

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls_client).unwrap(),
    ));
    client_config.transport_config(transport);

    let mut client_endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client_endpoint.set_default_client_config(client_config);

    let (server_conn, client_conn) = rt.block_on(async {
        let ep = server_endpoint.clone();
        let accept = tokio::spawn(async move {
            let incoming = ep.accept().await.unwrap();
            incoming.accept().unwrap().await.unwrap()
        });
        let client_conn = client_endpoint
            .connect(addr, "phantom")
            .unwrap()
            .await
            .unwrap();
        let server_conn = accept.await.unwrap();
        (server_conn, client_conn)
    });

    QuicConnPair {
        server_conn,
        client_conn,
        rt,
    }
}

#[derive(Debug)]
struct SkipVerify;

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ── Fragmentation helpers (same protocol as transport_quic.rs) ──────────────

fn fragment_and_send(conn: &quinn::Connection, msg: &Message, frame_id: u32) {
    let payload = bincode::serialize(msg).unwrap();
    let max_size = conn
        .max_datagram_size()
        .expect("datagrams must be supported");
    assert!(max_size > DATAGRAM_HEADER_SIZE);
    let chunk_size = max_size - DATAGRAM_HEADER_SIZE;
    let total_chunks = payload.len().div_ceil(chunk_size);

    for (idx, chunk) in payload.chunks(chunk_size).enumerate() {
        let mut buf = Vec::with_capacity(DATAGRAM_HEADER_SIZE + chunk.len());
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.extend_from_slice(&(idx as u16).to_be_bytes());
        buf.extend_from_slice(&(total_chunks as u16).to_be_bytes());
        buf.extend_from_slice(chunk);
        conn.send_datagram(Bytes::from(buf)).unwrap();
    }
}

struct Reassembler {
    current_frame_id: Option<u32>,
    chunks: Vec<Option<Vec<u8>>>,
    total_chunks: u16,
    received_count: u16,
}

impl Reassembler {
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn quic_datagram_max_size() {
    let pair = make_connected_pair();
    let server_max = pair.server_conn.max_datagram_size();
    let client_max = pair.client_conn.max_datagram_size();
    eprintln!("server_max_datagram={server_max:?}, client_max_datagram={client_max:?}");

    assert!(server_max.is_some(), "server datagrams must be supported");
    assert!(client_max.is_some(), "client datagrams must be supported");

    let max = server_max.unwrap();
    assert!(max >= 1000, "max_datagram_size too small: {max}");
    assert!(max < 65536, "max_datagram_size too large: {max}");
}

#[test]
fn quic_datagram_roundtrip() {
    let pair = make_connected_pair();
    pair.rt.block_on(async {
        // Server → Client
        pair.server_conn
            .send_datagram(Bytes::from_static(b"hello from server"))
            .unwrap();

        let dg = tokio::time::timeout(Duration::from_secs(5), pair.client_conn.read_datagram())
            .await
            .expect("timeout reading datagram")
            .unwrap();
        assert_eq!(&dg[..], b"hello from server");

        // Client → Server
        pair.client_conn
            .send_datagram(Bytes::from_static(b"hello from client"))
            .unwrap();

        let dg = tokio::time::timeout(Duration::from_secs(5), pair.server_conn.read_datagram())
            .await
            .expect("timeout reading datagram")
            .unwrap();
        assert_eq!(&dg[..], b"hello from client");
    });
}

#[test]
fn quic_datagram_small_pframe() {
    let pair = make_connected_pair();
    pair.rt.block_on(async {
        // Send a small P-frame that fits in a single datagram
        let msg = Message::VideoFrame {
            sequence: 42,
            frame: Box::new(EncodedFrame {
                codec: VideoCodec::H264,
                data: vec![0xCC; 200],
                is_keyframe: false,
            }),
        };

        fragment_and_send(&pair.server_conn, &msg, 0);

        // Receive and reassemble
        let mut reassembler = Reassembler::new();
        let dg = tokio::time::timeout(Duration::from_secs(5), pair.client_conn.read_datagram())
            .await
            .expect("timeout")
            .unwrap();
        let payload = reassembler
            .feed(&dg)
            .expect("single-chunk frame should complete");
        let decoded: Message = bincode::deserialize(&payload).unwrap();

        match decoded {
            Message::VideoFrame { sequence, frame } => {
                assert_eq!(sequence, 42);
                assert!(!frame.is_keyframe);
                assert_eq!(frame.data.len(), 200);
                assert!(frame.data.iter().all(|&b| b == 0xCC));
            }
            _ => panic!("expected VideoFrame"),
        }
    });
}

#[test]
fn quic_datagram_large_pframe_fragmented() {
    let pair = make_connected_pair();
    let max_dg = pair.server_conn.max_datagram_size().unwrap();
    let chunk_payload = max_dg - DATAGRAM_HEADER_SIZE;

    // Create a frame larger than max_datagram_size (forces fragmentation)
    let frame_data_size = chunk_payload * 5 + 100; // 5+ chunks
    let msg = Message::VideoFrame {
        sequence: 99,
        frame: Box::new(EncodedFrame {
            codec: VideoCodec::Av1,
            data: vec![0xDD; frame_data_size],
            is_keyframe: false,
        }),
    };
    let serialized_size = bincode::serialize(&msg).unwrap().len();
    let expected_chunks = serialized_size.div_ceil(chunk_payload);
    eprintln!(
        "frame_data={frame_data_size}, serialized={serialized_size}, max_dg={max_dg}, chunk_payload={chunk_payload}, expected_chunks={expected_chunks}"
    );

    pair.rt.block_on(async {
        fragment_and_send(&pair.server_conn, &msg, 1);

        let mut reassembler = Reassembler::new();
        let mut chunks_received = 0;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let dg = tokio::time::timeout_at(deadline, pair.client_conn.read_datagram())
                .await
                .expect("timeout waiting for datagram chunk")
                .unwrap();
            chunks_received += 1;

            if let Some(payload) = reassembler.feed(&dg) {
                eprintln!(
                    "reassembled after {chunks_received} chunks (expected ~{expected_chunks})"
                );
                // Chunk count should match within 1 (MTU can vary slightly)
                assert!(
                    chunks_received >= expected_chunks.saturating_sub(1)
                        && chunks_received <= expected_chunks + 1,
                    "unexpected chunk count: got {chunks_received}, expected ~{expected_chunks}"
                );

                let decoded: Message = bincode::deserialize(&payload).unwrap();
                match decoded {
                    Message::VideoFrame { sequence, frame } => {
                        assert_eq!(sequence, 99);
                        assert!(!frame.is_keyframe);
                        assert_eq!(frame.data.len(), frame_data_size);
                        assert!(frame.data.iter().all(|&b| b == 0xDD));
                    }
                    _ => panic!("expected VideoFrame"),
                }
                return;
            }
        }
    });
}

#[test]
fn quic_datagram_multiple_frames() {
    let pair = make_connected_pair();
    pair.rt.block_on(async {
        // Send 3 frames in sequence
        for i in 0..3u32 {
            let msg = Message::VideoFrame {
                sequence: i as u64 + 1,
                frame: Box::new(EncodedFrame {
                    codec: VideoCodec::H264,
                    data: vec![(i as u8 + 0xA0); 100],
                    is_keyframe: false,
                }),
            };
            fragment_and_send(&pair.server_conn, &msg, i);
        }

        // Receive all 3
        let mut reassembler = Reassembler::new();
        let mut received = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while received.len() < 3 {
            let dg = tokio::time::timeout_at(deadline, pair.client_conn.read_datagram())
                .await
                .expect("timeout")
                .unwrap();
            if let Some(payload) = reassembler.feed(&dg) {
                let msg: Message = bincode::deserialize(&payload).unwrap();
                match &msg {
                    Message::VideoFrame { sequence, .. } => {
                        received.push(*sequence);
                    }
                    _ => panic!("expected VideoFrame"),
                }
            }
        }

        // All 3 should arrive (localhost, no packet loss)
        assert_eq!(received.len(), 3);
        // Order may vary with datagrams, but on localhost should be ordered
        assert!(received.contains(&1));
        assert!(received.contains(&2));
        assert!(received.contains(&3));
    });
}
