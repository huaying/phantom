use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use str0m::{Candidate, Rtc};
use tungstenite::WebSocket;

/// Generate a self-signed TLS config for HTTPS (enables WebCodecs on non-localhost).
fn make_tls_acceptor() -> Result<Arc<rustls::ServerConfig>> {
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "Phantom Remote Desktop");
    // Add SANs for any IP
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".try_into()?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
    ];
    let cert = params.self_signed(&key_pair)?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|e| anyhow::anyhow!("key: {e}"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(Arc::new(config))
}

/// Embedded static files for the web client.
/// Only included when the WASM files exist (built via `wasm-pack build crates/web`).
const INDEX_HTML: &str = include_str!("../web/index.html");

#[cfg(feature = "web-client")]
const WASM_JS: &[u8] = include_bytes!("../../web/pkg/phantom_web.js");
#[cfg(feature = "web-client")]
const WASM_BIN: &[u8] = include_bytes!("../../web/pkg/phantom_web_bg.wasm");

#[cfg(not(feature = "web-client"))]
const WASM_JS: &[u8] = b"console.error('web client not compiled: build with --features web-client')";
#[cfg(not(feature = "web-client"))]
const WASM_BIN: &[u8] = b"";

/// Combined web server: HTTP static files + WebSocket fallback + WebRTC via POST /rtc.
type SessionPair = (super::transport_webrtc::WebRtcSender, super::transport_webrtc::WebRtcReceiver);

#[allow(dead_code)]
pub struct WebServerTransport {
    rtc_session: Arc<Mutex<Option<SessionPair>>>,
    rtc_notify: mpsc::Receiver<()>,
    /// WebSocket fallback (future: adaptive WS/WebRTC)
    ws_rx: mpsc::Receiver<WsConnection>,
}

#[allow(dead_code)]
pub struct WsConnection {
    pub data_sender: WsSender,
    pub data_receiver: WsReceiver,
}

impl WebServerTransport {
    pub fn start(http_port: u16, ws_port: u16, udp_port: u16) -> Result<Self> {
        // ICE candidate IP: use PHANTOM_HOST env var, or detect, or fallback to 127.0.0.1
        // No UDP socket created here — each session creates its own in run_rtc()
        let host_ip: std::net::IpAddr = std::env::var("PHANTOM_HOST")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| get_local_ip().unwrap_or([127, 0, 0, 1].into()));
        let candidate_addr = std::net::SocketAddr::new(host_ip, udp_port);
        tracing::info!(%candidate_addr, "WebRTC ICE candidate address");

        // Channel: POST /rtc → run loop (raw Rtc instances)
        let (rtc_tx, rtc_rx) = mpsc::channel::<Rtc>();
        // Shared session slot: run_loop writes latest, main thread takes it
        let rtc_session: Arc<Mutex<Option<SessionPair>>> = Arc::new(Mutex::new(None));
        let rtc_session2 = rtc_session.clone();
        let (notify_tx, rtc_notify) = mpsc::channel::<()>();

        // Start WebRTC run loop
        std::thread::spawn(move || {
            super::transport_webrtc::run_loop(candidate_addr, rtc_rx, rtc_session2, notify_tx);
        });
        // Channel for WS fallback connections
        let (ws_tx, ws_rx) = mpsc::channel::<WsConnection>();

        // HTTPS server thread (serves static files + POST /rtc)
        // Self-signed TLS enables WebCodecs (VideoDecoder) on non-localhost origins.
        let tls_acceptor = make_tls_acceptor()?;
        let http_addr = format!("0.0.0.0:{http_port}");
        let http_listener = TcpListener::bind(&http_addr).context("bind HTTPS")?;
        tracing::info!(addr = %http_addr, "HTTPS server (static + POST /rtc)");

        std::thread::spawn(move || {
            for tcp_stream in http_listener.incoming().flatten() {
                let tls = tls_acceptor.clone();
                let rtc_tx = rtc_tx.clone();
                let candidate_addr = candidate_addr;
                std::thread::spawn(move || {
                    let conn = match rustls::ServerConnection::new(tls) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
                    // Wrap in a BufReader-compatible type for handle_http
                    let _ = handle_http_rw(&mut stream, rtc_tx, candidate_addr);
                });
            }
        });

        // WebSocket server thread (fallback)
        let ws_addr = format!("0.0.0.0:{ws_port}");
        let ws_listener = TcpListener::bind(&ws_addr).context("bind WS")?;
        tracing::info!(addr = %ws_addr, "WebSocket server (fallback)");

        std::thread::spawn(move || {
            for stream in ws_listener.incoming().flatten() {
                let _ = stream.set_nodelay(true);
                let peer = stream.peer_addr().ok();
                match tungstenite::accept(stream) {
                    Ok(ws) => {
                        tracing::info!(?peer, "WebSocket fallback connected");
                        let (send_tx, send_rx) = mpsc::channel();
                        let (recv_tx, recv_rx) = mpsc::channel();
                        std::thread::spawn(move || ws_io_loop(ws, send_rx, recv_tx));
                        let _ = ws_tx.send(WsConnection {
                            data_sender: WsSender { tx: send_tx },
                            data_receiver: WsReceiver { rx: recv_rx },
                        });
                    }
                    Err(e) => tracing::debug!(?peer, "WS handshake failed: {e}"),
                }
            }
        });

        Ok(Self { rtc_session, rtc_notify, ws_rx })
    }

    /// Accept: WebRTC. Blocks until a session is ready. Always gets the latest.
    pub fn accept_webrtc(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        loop {
            // Wait for notification
            let _ = self.rtc_notify.recv_timeout(std::time::Duration::from_millis(100));
            // Take the latest session (run_loop may have overwritten multiple times)
            if let Some((s, r)) = self.rtc_session.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Ok((Box::new(s), Box::new(r)));
            }
        }
    }

    /// Accept: WebSocket only (fallback mode).
    #[allow(dead_code)]
    pub fn accept_ws(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        let ws = self.ws_rx.recv().context("WS channel closed")?;
        tracing::info!("WebSocket client accepted");
        Ok((Box::new(ws.data_sender), Box::new(ws.data_receiver)))
    }
}

/// Handle a single HTTP request over any Read+Write stream (plain TCP or TLS).
fn handle_http_rw(
    stream: &mut (impl Read + Write),
    rtc_tx: mpsc::Sender<Rtc>,
    candidate_addr: std::net::SocketAddr,
) -> Result<()> {
    // Read full request (headers + body) into buffer
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    buf.truncate(n);
    let request = String::from_utf8_lossy(&buf);

    let method = request.split_whitespace().next().unwrap_or("GET");
    let path = request.split_whitespace().nth(1).unwrap_or("/");

    // Find body after \r\n\r\n
    let body_start = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(n);

    match (method, path) {
        ("POST", "/rtc") => {
            let body = &buf[body_start..];
            let offer_json: serde_json::Value = serde_json::from_slice(body)?;
            let sdp_str = offer_json["sdp"].as_str().context("missing sdp")?;

            let mut rtc = Rtc::builder().build();
            let candidate = Candidate::host(candidate_addr, "udp").context("host candidate")?;
            rtc.add_local_candidate(candidate);

            let offer = str0m::change::SdpOffer::from_sdp_string(sdp_str).context("parse SDP")?;
            let answer = rtc.sdp_api().accept_offer(offer).context("accept offer")?;
            let answer_json = serde_json::json!({ "type": "answer", "sdp": answer.to_sdp_string() });

            rtc_tx.send(rtc).map_err(|_| anyhow::anyhow!("rtc channel closed"))?;

            let resp_body = answer_json.to_string();
            write!(stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                resp_body.len()
            )?;
            stream.write_all(resp_body.as_bytes())?;
            tracing::info!("SDP offer/answer exchanged via POST /rtc");
        }
        ("OPTIONS", "/rtc") => {
            write!(stream,
                "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n"
            )?;
        }
        _ => {
            let (status, content_type, body): (&str, &str, &[u8]) = match path {
                "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
                "/phantom_web.js" => ("200 OK", "application/javascript", WASM_JS),
                "/phantom_web_bg.wasm" => ("200 OK", "application/wasm", WASM_BIN),
                _ => ("404 Not Found", "text/plain", b"404 Not Found"),
            };
            write!(stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
        }
    }
    stream.flush()?;
    Ok(())
}


fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

// -- WebSocket fallback IO loop --

fn ws_io_loop(
    mut ws: WebSocket<TcpStream>,
    send_rx: mpsc::Receiver<Vec<u8>>,
    recv_tx: mpsc::Sender<Vec<u8>>,
) {
    if let Ok(stream) = ws.get_ref().try_clone() {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(5)));
    }
    loop {
        while let Ok(data) = send_rx.try_recv() {
            if ws.send(tungstenite::Message::Binary(data)).is_err() { return; }
        }
        match ws.read() {
            Ok(tungstenite::Message::Binary(data)) => {
                if recv_tx.send(data).is_err() { return; }
            }
            Ok(tungstenite::Message::Close(_)) => return,
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    }
}

pub struct WsSender { tx: mpsc::Sender<Vec<u8>> }
impl MessageSender for WsSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
        self.tx.send(payload).map_err(|_| anyhow::anyhow!("ws closed"))
    }
}

pub struct WsReceiver { rx: mpsc::Receiver<Vec<u8>> }
impl MessageReceiver for WsReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        let data = self.rx.recv().context("ws closed")?;
        bincode::deserialize(&data).context("deserialize")
    }
}
