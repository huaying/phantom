use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use quinn::Endpoint;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::runtime::Runtime;

/// QUIC client transport using quinn.
pub struct QuicClientTransport {
    rt: Arc<Runtime>,
}

impl QuicClientTransport {
    pub fn new() -> Result<Self> {
        let rt = Runtime::new().context("create tokio runtime")?;
        Ok(Self { rt: Arc::new(rt) })
    }

    pub fn connect(&self, addr: &str) -> Result<(QuicSender, QuicReceiver)> {
        let server_addr: SocketAddr = addr.parse().context("parse server address")?;

        let conn = self.rt.block_on(async {
            // Client endpoint: bind to any local port
            let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
                .context("create client endpoint")?;

            // Accept any server certificate (like SSH first-connect)
            let tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
                .with_no_client_auth();

            let mut client_config = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
                    .map_err(|e| anyhow::anyhow!("QUIC client config: {e}"))?
            ));

            // Keep connection alive
            let mut transport = quinn::TransportConfig::default();
            transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
            client_config.transport_config(Arc::new(transport));

            endpoint.set_default_client_config(client_config);

            let conn = endpoint.connect(server_addr, "phantom")
                .context("initiate QUIC connection")?
                .await
                .context("QUIC handshake failed")?;

            tracing::info!(peer = %conn.remote_address(), "QUIC connected");
            Ok::<_, anyhow::Error>(conn)
        })?;

        // Accept the bidirectional stream opened by the server
        let (send_stream, recv_stream) = self.rt.block_on(async {
            conn.accept_bi().await.context("accept bidirectional stream from server")
        })?;

        // Note: server opens the stream, client accepts. The send/recv are swapped:
        // server's send = client's recv, server's recv = client's send
        Ok((
            QuicSender { rt: self.rt.clone(), stream: send_stream },
            QuicReceiver { rt: self.rt.clone(), stream: recv_stream },
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

// -- Skip server certificate verification (like SSH first-connect) --

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
        &self, _message: &[u8], _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _message: &[u8], _cert: &rustls::pki_types::CertificateDer<'_>,
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
