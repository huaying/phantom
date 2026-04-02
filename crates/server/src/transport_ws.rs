use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use tungstenite::WebSocket;

#[cfg(feature = "webrtc")]
use std::sync::Mutex;
#[cfg(feature = "webrtc")]
use std::time::Instant;

/// Load or generate a self-signed TLS cert for HTTPS (enables WebCodecs on non-localhost).
/// Cert is persisted to ~/.phantom_cert.pem + ~/.phantom_key.pem so the browser
/// only needs to accept it once. Survives server restarts.
fn make_tls_acceptor() -> Result<Arc<rustls::ServerConfig>> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let dir = std::path::PathBuf::from(&home).join(".phantom");
    let _ = std::fs::create_dir_all(&dir);
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let (cert_der, key_der) = if cert_path.exists() && key_path.exists() {
        // Load existing cert
        let cert_pem = std::fs::read(&cert_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let cert = rustls_pemfile::certs(&mut &cert_pem[..])
            .next().ok_or_else(|| anyhow::anyhow!("no cert in PEM"))??;
        let key = rustls_pemfile::private_key(&mut &key_pem[..])?.ok_or_else(|| anyhow::anyhow!("no key in PEM"))?;
        tracing::info!("loaded TLS cert from {}", cert_path.display());
        (cert, key)
    } else {
        // Generate new cert and save
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, "Phantom Remote Desktop");
        params.subject_alt_names = vec![
            rcgen::SanType::DnsName("localhost".try_into()?),
            rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        ];
        let cert = params.self_signed(&key_pair)?;

        // Save as PEM for next restart (best-effort, not fatal if it fails)
        if let Err(e) = std::fs::write(&cert_path, cert.pem()) {
            tracing::warn!("could not save cert to {}: {e}", cert_path.display());
        } else if let Err(e) = std::fs::write(&key_path, key_pair.serialize_pem()) {
            tracing::warn!("could not save key to {}: {e}", key_path.display());
        } else {
            tracing::info!("generated TLS cert, saved to {}", cert_path.display());
        }

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow::anyhow!("key: {e}"))?;
        (cert_der, key_der)
    };

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(Arc::new(config))
}

/// Embedded static files for the web client.
const INDEX_HTML: &str = include_str!("../web/index.html");

#[cfg(feature = "web-client")]
const WASM_JS: &[u8] = include_bytes!("../../web/pkg/phantom_web.js");
#[cfg(feature = "web-client")]
const WASM_BIN: &[u8] = include_bytes!("../../web/pkg/phantom_web_bg.wasm");

#[cfg(not(feature = "web-client"))]
const WASM_JS: &[u8] = b"console.error('web client not compiled: build with --features web-client')";
#[cfg(not(feature = "web-client"))]
const WASM_BIN: &[u8] = b"";

// --- WebRTC types (only when feature enabled) ---

#[cfg(feature = "webrtc")]
type SessionPair = (super::transport_webrtc::WebRtcSender, super::transport_webrtc::WebRtcReceiver);

#[allow(dead_code)]
pub struct WebServerTransport {
    #[cfg(feature = "webrtc")]
    rtc_session: Arc<Mutex<Option<SessionPair>>>,
    #[cfg(feature = "webrtc")]
    rtc_notify: mpsc::Receiver<()>,
    ws_rx: mpsc::Receiver<WsConnection>,
}

#[allow(dead_code)]
pub struct WsConnection {
    pub data_sender: WsSender,
    pub data_receiver: WsReceiver,
}

impl WebServerTransport {
    pub fn start(http_port: u16, _ws_port: u16, _udp_port: u16) -> Result<Self> {
        // Channel for WS connections
        let (ws_tx, ws_rx) = mpsc::channel::<WsConnection>();

        // --- WebRTC setup (only when feature enabled) ---
        #[cfg(feature = "webrtc")]
        let (rtc_session, rtc_notify) = {
            use str0m::Rtc;

            let host_ip: std::net::IpAddr = std::env::var("PHANTOM_HOST")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| get_local_ip().unwrap_or([127, 0, 0, 1].into()));
            let candidate_addr = std::net::SocketAddr::new(host_ip, _udp_port);
            tracing::info!(%candidate_addr, "WebRTC ICE candidate address");

            let (rtc_tx, rtc_rx) = mpsc::channel::<Rtc>();
            let rtc_session: Arc<Mutex<Option<SessionPair>>> = Arc::new(Mutex::new(None));
            let rtc_session2 = rtc_session.clone();
            let (notify_tx, rtc_notify) = mpsc::channel::<()>();

            std::thread::spawn(move || {
                super::transport_webrtc::run_loop(candidate_addr, rtc_rx, rtc_session2, notify_tx);
            });

            // HTTPS server thread (serves static files + POST /rtc + WSS upgrade)
            let tls_acceptor = make_tls_acceptor()?;
            let http_addr = format!("0.0.0.0:{http_port}");
            let http_listener = TcpListener::bind(&http_addr).context("bind HTTPS")?;
            tracing::info!(addr = %http_addr, "HTTPS server (static + POST /rtc + WSS)");

            std::thread::spawn(move || {
                for tcp_stream in http_listener.incoming().flatten() {
                    let tls = tls_acceptor.clone();
                    let rtc_tx = rtc_tx.clone();
                    let ws_tx = ws_tx.clone();
                    let candidate_addr = candidate_addr;
                    std::thread::spawn(move || {
                        let conn = match rustls::ServerConnection::new(tls) {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
                        match handle_http_rw_rtc(&mut stream, rtc_tx, candidate_addr) {
                            Ok(HttpResult::WsUpgrade) => {
                                spawn_ws_connection(stream, ws_tx);
                            }
                            _ => {}
                        }
                    });
                }
            });

            (rtc_session, rtc_notify)
        };

        // --- No WebRTC: just HTTPS + WSS ---
        #[cfg(not(feature = "webrtc"))]
        {
            let tls_acceptor = make_tls_acceptor()?;
            let http_addr = format!("0.0.0.0:{http_port}");
            let http_listener = TcpListener::bind(&http_addr).context("bind HTTPS")?;
            tracing::info!(addr = %http_addr, "HTTPS server (static + WSS)");

            std::thread::spawn(move || {
                for tcp_stream in http_listener.incoming().flatten() {
                    let tls = tls_acceptor.clone();
                    let ws_tx = ws_tx.clone();
                    std::thread::spawn(move || {
                        let conn = match rustls::ServerConnection::new(tls) {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        let mut stream = rustls::StreamOwned::new(conn, tcp_stream);
                        match handle_http_rw(&mut stream) {
                            Ok(HttpResult::WsUpgrade) => {
                                spawn_ws_connection(stream, ws_tx);
                            }
                            _ => {}
                        }
                    });
                }
            });
        }

        Ok(Self {
            #[cfg(feature = "webrtc")]
            rtc_session,
            #[cfg(feature = "webrtc")]
            rtc_notify,
            ws_rx,
        })
    }

    /// Accept: WebSocket (default). Blocks until a WS client connects.
    #[allow(dead_code)]
    pub fn accept_ws(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        let ws = self.ws_rx.recv().context("WS channel closed")?;
        tracing::info!("WebSocket client accepted");
        Ok((Box::new(ws.data_sender), Box::new(ws.data_receiver)))
    }

    /// Accept: either WebRTC or WebSocket, whichever connects first.
    #[cfg(feature = "webrtc")]
    pub fn accept_any(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        loop {
            let _ = self.rtc_notify.recv_timeout(std::time::Duration::from_millis(50));
            if let Some((s, r)) = self.rtc_session.lock().unwrap_or_else(|e| e.into_inner()).take() {
                tracing::info!("accepted WebRTC client");
                return Ok((Box::new(s), Box::new(r)));
            }
            if let Ok(ws) = self.ws_rx.try_recv() {
                tracing::info!("accepted WebSocket client");
                return Ok((Box::new(ws.data_sender), Box::new(ws.data_receiver)));
            }
        }
    }
}

// --- HTTP handling ---

enum HttpResult {
    Done,
    WsUpgrade,
}

/// HTTP handler WITHOUT WebRTC (no POST /rtc).
#[cfg(not(feature = "webrtc"))]
fn handle_http_rw(
    stream: &mut (impl Read + Write),
) -> Result<HttpResult> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    buf.truncate(n);
    let request = String::from_utf8_lossy(&buf);
    let raw_path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    // WebSocket upgrade
    let request_lower = request.to_ascii_lowercase();
    if request_lower.contains("upgrade: websocket") {
        return send_ws_upgrade(stream, &request);
    }

    serve_static(stream, path)?;
    Ok(HttpResult::Done)
}

/// HTTP handler WITH WebRTC (POST /rtc + static + WSS upgrade).
#[cfg(feature = "webrtc")]
fn handle_http_rw_rtc(
    stream: &mut (impl Read + Write),
    rtc_tx: mpsc::Sender<str0m::Rtc>,
    candidate_addr: std::net::SocketAddr,
) -> Result<HttpResult> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    buf.truncate(n);
    let request = String::from_utf8_lossy(&buf);
    let method = request.split_whitespace().next().unwrap_or("GET");
    let raw_path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    // WebSocket upgrade
    let request_lower = request.to_ascii_lowercase();
    if request_lower.contains("upgrade: websocket") {
        return send_ws_upgrade(stream, &request);
    }

    let body_start = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(n);

    match (method, path) {
        ("POST", "/rtc") => {
            use str0m::{Candidate, Rtc};
            let body = &buf[body_start..];
            let offer_json: serde_json::Value = serde_json::from_slice(body)?;
            let sdp_str = offer_json["sdp"].as_str().context("missing sdp")?;

            let mut rtc = Rtc::builder().build(Instant::now());
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
        _ => { serve_static(stream, path)?; }
    }
    stream.flush()?;
    Ok(HttpResult::Done)
}

fn send_ws_upgrade(stream: &mut (impl Read + Write), request: &str) -> Result<HttpResult> {
    let key = request.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    write!(stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(HttpResult::WsUpgrade)
}

fn serve_static(stream: &mut (impl Read + Write), path: &str) -> Result<()> {
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
    stream.flush()?;
    Ok(())
}

fn spawn_ws_connection(
    stream: rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    ws_tx: mpsc::Sender<WsConnection>,
) {
    let _ = stream.sock.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    let ws = tungstenite::WebSocket::from_raw_socket(
        stream, tungstenite::protocol::Role::Server, None,
    );
    tracing::info!("WebSocket client connected via HTTPS port");
    let (send_tx, send_rx) = mpsc::channel();
    let (recv_tx, recv_rx) = mpsc::channel();
    std::thread::spawn(move || ws_io_loop(ws, send_rx, recv_tx));
    let _ = ws_tx.send(WsConnection {
        data_sender: WsSender { tx: send_tx },
        data_receiver: WsReceiver { rx: recv_rx },
    });
}

#[cfg(feature = "webrtc")]
fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

// -- WebSocket IO loop --

fn ws_io_loop<S: std::io::Read + std::io::Write>(
    mut ws: WebSocket<S>,
    send_rx: mpsc::Receiver<Vec<u8>>,
    recv_tx: mpsc::Sender<Vec<u8>>,
) {
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
