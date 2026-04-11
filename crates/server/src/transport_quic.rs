use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use quinn::{Endpoint, ServerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// QUIC server transport using quinn.
/// Video frames and input events use separate QUIC streams for independent flow control.
pub struct QuicServerTransport {
    rt: Arc<Runtime>,
    endpoint: Endpoint,
}

impl QuicServerTransport {
    pub fn bind(addr: &str) -> Result<Self> {
        let rt = Runtime::new().context("create tokio runtime")?;
        let addr: SocketAddr = addr.parse().context("parse address")?;

        let (server_config, cert_der) = generate_self_signed_config()?;
        let endpoint = rt.block_on(async {
            Endpoint::server(server_config, addr)
        }).context("bind QUIC endpoint")?;

        let fingerprint = ring_fingerprint(&cert_der);
        tracing::info!(%addr, fingerprint, "QUIC server listening (self-signed cert)");

        Ok(Self { rt: Arc::new(rt), endpoint })
    }

    /// Accept a connection, return (sender, receiver) on separate streams.
    pub fn accept(&self) -> Result<(QuicSender, QuicReceiver)> {
        let rt = self.rt.clone();
        let conn = rt.block_on(async {
            let incoming = self.endpoint.accept().await
                .context("no incoming connection")?;
            incoming.await.context("QUIC handshake failed")
        })?;

        let peer = conn.remote_address();
        tracing::info!(%peer, "QUIC client connected");

        // Open a bidirectional stream for video (server→client) and input (client→server)
        let (send_stream, recv_stream) = rt.block_on(async {
            conn.open_bi().await.context("open bidirectional stream")
        })?;

        Ok((
            QuicSender { rt: rt.clone(), stream: send_stream },
            QuicReceiver { rt: rt.clone(), stream: recv_stream },
        ))
    }
}

pub struct QuicSender {
    rt: Arc<Runtime>,
    stream: quinn::SendStream,
}

impl MessageSender for QuicSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
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
}

impl MessageReceiver for QuicReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        self.rt.block_on(async {
            let mut len_buf = [0u8; 4];
            self.stream.read_exact(&mut len_buf).await
                .context("read message length")?;
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 64 * 1024 * 1024 {
                anyhow::bail!("message too large ({len} bytes)");
            }
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).await
                .context("read message payload")?;
            bincode::deserialize(&payload).context("deserialize")
        })
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

    let server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .map_err(|e| anyhow::anyhow!("QUIC server config: {e}"))?
    ));

    Ok((server_config, cert_der))
}

fn ring_fingerprint(cert_der: &[u8]) -> String {
    use std::fmt::Write;
    // Simple SHA-256 fingerprint for display
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut s = String::new();
    for (i, b) in digest.as_ref().iter().enumerate() {
        if i > 0 { s.push(':'); }
        // write! to String is infallible
        let _ = write!(s, "{:02X}", b);
    }
    s
}
