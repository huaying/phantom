use super::ws_audio::{AudioRoutes, SessionAudio};
use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "webrtc")]
use std::sync::Mutex;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tungstenite::WebSocket;

// ── JWT token authentication ────────────────────────────────────────────────

/// Extract a query parameter value from a raw HTTP path (e.g. "/ws?token=abc").
fn extract_query_param<'a>(raw_path: &'a str, key: &str) -> Option<&'a str> {
    raw_path.split('?').nth(1).and_then(|query| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            if k == key {
                Some(v)
            } else {
                None
            }
        })
    })
}

/// Verify a JWT token signed with HMAC-SHA256.
/// Returns (sub, vm_id) claims on success.
fn verify_jwt(token: &str, secret: &[u8]) -> Result<(String, String)> {
    use base64::Engine;
    let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        anyhow::bail!("invalid JWT: expected 3 parts");
    }
    let (header_b64, payload_b64, sig_b64) = (parts[0], parts[1], parts[2]);

    // Verify HMAC-SHA256 signature
    let signing_input = format!("{header_b64}.{payload_b64}");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let signature = url_safe.decode(sig_b64).context("decode JWT signature")?;
    ring::hmac::verify(&key, signing_input.as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("invalid JWT signature"))?;

    // Parse header — verify alg
    let header: serde_json::Value =
        serde_json::from_slice(&url_safe.decode(header_b64).context("decode JWT header")?)?;
    if header["alg"].as_str() != Some("HS256") {
        anyhow::bail!("unsupported JWT algorithm: {}", header["alg"]);
    }

    // Parse claims
    let claims: serde_json::Value =
        serde_json::from_slice(&url_safe.decode(payload_b64).context("decode JWT payload")?)?;

    // Check expiration
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if let Some(exp) = claims["exp"].as_u64() {
        if now > exp {
            anyhow::bail!("JWT expired");
        }
    } else {
        anyhow::bail!("JWT missing exp claim");
    }
    // Check not-before (optional)
    if let Some(nbf) = claims["nbf"].as_u64() {
        if now < nbf {
            anyhow::bail!("JWT not yet valid");
        }
    }

    let sub = claims["sub"].as_str().unwrap_or("unknown").to_string();
    let vm_id = claims["vm_id"].as_str().unwrap_or("").to_string();
    Ok((sub, vm_id))
}

/// Check JWT auth on an HTTP request that carries `?token=...`.
/// Returns Ok(()) if auth passes or if no auth is configured.
/// Returns Err with HTTP 401 already sent on failure.
fn check_request_auth(
    stream: &mut (impl Read + Write),
    raw_path: &str,
    auth_secret: &Option<Vec<u8>>,
) -> Result<()> {
    let secret = match auth_secret {
        Some(s) => s,
        None => return Ok(()), // No auth configured
    };
    match extract_query_param(raw_path, "token") {
        Some(token) => match verify_jwt(token, secret) {
            Ok((user, vm_id)) => {
                tracing::info!(user, vm_id, "authenticated HTTP request");
                crate::sso::on_jwt_verified(&user);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("JWT auth failed: {e}");
                let _ = write!(
                    stream,
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nUnauthorized"
                );
                let _ = stream.flush();
                anyhow::bail!("auth failed");
            }
        },
        None => {
            tracing::warn!("request missing ?token= param");
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nUnauthorized"
            );
            let _ = stream.flush();
            anyhow::bail!("auth required");
        }
    }
}

/// Maximum concurrent HTTP/WS handler threads.
const MAX_CONNECTIONS: usize = 16;

/// Keep-alive idle timeout: close the connection if no new request arrives
/// within this duration.
const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum requests served on a single keep-alive connection before we close
/// it (prevents a single connection from monopolising a pool slot forever).
const MAX_REQUESTS_PER_CONN: usize = 100;

// ── Connection pool guard ───────────────────────────────────────────────────

/// RAII guard that increments on creation and decrements on drop, keeping
/// the active-connection count accurate regardless of how the handler exits.
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    /// Try to acquire a slot. Returns `None` if the pool is full.
    fn try_acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        loop {
            let current = counter.load(Ordering::Relaxed);
            if current >= MAX_CONNECTIONS {
                return None;
            }
            if counter
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Self(Arc::clone(counter)));
            }
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

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
            .next()
            .ok_or_else(|| anyhow::anyhow!("no cert in PEM"))??;
        let key = rustls_pemfile::private_key(&mut &key_pem[..])?
            .ok_or_else(|| anyhow::anyhow!("no key in PEM"))?;
        tracing::info!("loaded TLS cert from {}", cert_path.display());
        (cert, key)
    } else {
        // Generate new cert and save
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Phantom Remote Desktop");
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

    // Pass provider explicitly. rustls 0.23 ServerConfig::builder() panics if
    // no default provider is installed, even if the `ring` feature is enabled.
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("rustls protocol versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(Arc::new(config))
}

/// Embedded static files for the web client.
const INDEX_HTML: &str = include_str!("../../web/index.html");

#[cfg(feature = "web-client")]
const WASM_JS: &[u8] = include_bytes!("../../../web/pkg/phantom_web.js");
#[cfg(feature = "web-client")]
const WASM_BIN: &[u8] = include_bytes!("../../../web/pkg/phantom_web_bg.wasm");

#[cfg(not(feature = "web-client"))]
const WASM_JS: &[u8] =
    b"console.error('web client not compiled: build with --features web-client')";
#[cfg(not(feature = "web-client"))]
const WASM_BIN: &[u8] = b"";

// --- WebRTC types (only when feature enabled) ---

#[cfg(feature = "webrtc")]
type SessionPair = (super::webrtc::WebRtcSender, super::webrtc::WebRtcReceiver);

#[cfg(feature = "webrtc")]
type RtcRequest = super::webrtc::PendingRtcSession;

#[allow(dead_code)]
pub struct WebServerTransport {
    #[cfg(feature = "webrtc")]
    rtc_session: Arc<Mutex<Option<SessionPair>>>,
    #[cfg(feature = "webrtc")]
    rtc_notify: mpsc::Receiver<()>,
    ws_rx: mpsc::Receiver<WsConnection>,
    audio_ws_rx: mpsc::Receiver<WsSender>,
}

#[allow(dead_code)]
pub struct WsConnection {
    pub data_sender: WsSender,
    pub data_receiver: WsReceiver,
}

impl WebServerTransport {
    pub fn start(
        http_port: u16,
        _ws_port: u16,
        _udp_port: u16,
        auth_secret: Option<Vec<u8>>,
    ) -> Result<Self> {
        let auth_secret = Arc::new(auth_secret);
        // Channel for WS connections
        let (ws_tx, ws_rx) = mpsc::channel::<WsConnection>();
        let (audio_ws_tx, audio_ws_rx) = mpsc::channel::<WsSender>();
        let audio_routes = AudioRoutes::default();

        // Shared connection counter for the thread pool
        let conn_count = Arc::new(AtomicUsize::new(0));

        // --- WebRTC setup (only when feature enabled) ---
        #[cfg(feature = "webrtc")]
        let (rtc_session, rtc_notify) = {
            let host_ip: std::net::IpAddr = std::env::var("PHANTOM_HOST")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| get_local_ip().unwrap_or([127, 0, 0, 1].into()));
            let candidate_addr = std::net::SocketAddr::new(host_ip, _udp_port);
            tracing::info!(%candidate_addr, "WebRTC ICE candidate address");

            let (rtc_tx, rtc_rx) = mpsc::channel::<RtcRequest>();
            let rtc_session: Arc<Mutex<Option<SessionPair>>> = Arc::new(Mutex::new(None));
            let rtc_session2 = rtc_session.clone();
            let (notify_tx, rtc_notify) = mpsc::channel::<()>();

            std::thread::spawn(move || {
                super::webrtc::run_loop(candidate_addr, rtc_rx, rtc_session2, notify_tx);
            });

            // HTTPS server thread (serves static files + POST /rtc + WSS upgrade)
            let tls_acceptor = make_tls_acceptor()?;
            let http_addr = format!("0.0.0.0:{http_port}");
            let http_listener = TcpListener::bind(&http_addr).context("bind HTTPS")?;
            tracing::info!(addr = %http_addr, "HTTPS server (static + POST /rtc + WSS)");

            let pool = conn_count.clone();
            std::thread::spawn(move || {
                for tcp_stream in http_listener.incoming().flatten() {
                    let guard = match ConnGuard::try_acquire(&pool) {
                        Some(g) => g,
                        None => {
                            // Pool full — reject with 503
                            tracing::warn!("connection pool full ({MAX_CONNECTIONS}), rejecting");
                            let _ = tcp_stream.shutdown(std::net::Shutdown::Both);
                            continue;
                        }
                    };

                    let tls = tls_acceptor.clone();
                    let rtc_tx = rtc_tx.clone();
                    let ws_tx = ws_tx.clone();
                    let audio_ws_tx = audio_ws_tx.clone();
                    let audio_routes = audio_routes.clone();
                    let candidate_addr = candidate_addr;
                    let auth = auth_secret.clone();
                    std::thread::Builder::new()
                        .name("http-handler".into())
                        .spawn(move || {
                            let _guard = guard; // held until this thread exits
                            let conn = match rustls::ServerConnection::new(tls) {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::warn!("ServerConnection::new failed: {e}");
                                    #[cfg(target_os = "windows")]
                                    crate::service_win::svc_log(&format!(
                                        "ServerConnection::new failed: {e}"
                                    ));
                                    return;
                                }
                            };
                            let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

                            // Set read timeout for keep-alive idle detection
                            let _ = stream.sock.set_read_timeout(Some(KEEPALIVE_TIMEOUT));

                            // Keep-alive loop: serve multiple requests on the same TLS connection
                            for _req_num in 0..MAX_REQUESTS_PER_CONN {
                                match handle_http_rw_rtc(
                                    &mut stream,
                                    &rtc_tx,
                                    candidate_addr,
                                    &auth,
                                ) {
                                    Ok(HttpResult::WsUpgrade) => {
                                        spawn_ws_connection(stream, ws_tx, audio_routes);
                                        return;
                                    }
                                    Ok(HttpResult::WsUpgradeAudio(session)) => {
                                        spawn_audio_ws_connection(
                                            stream,
                                            audio_ws_tx,
                                            audio_routes,
                                            session,
                                        );
                                        return;
                                    }
                                    Ok(HttpResult::Done) => continue,
                                    Ok(HttpResult::Close) => break,
                                    Err(e) => {
                                        tracing::warn!("HTTP/WebRTC handler error: {e:#}");
                                        break;
                                    }
                                }
                            }
                        })
                        .ok();
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

            let pool = conn_count.clone();
            std::thread::spawn(move || {
                for tcp_stream in http_listener.incoming().flatten() {
                    let guard = match ConnGuard::try_acquire(&pool) {
                        Some(g) => g,
                        None => {
                            tracing::warn!("connection pool full ({MAX_CONNECTIONS}), rejecting");
                            let _ = tcp_stream.shutdown(std::net::Shutdown::Both);
                            continue;
                        }
                    };

                    let tls = tls_acceptor.clone();
                    let ws_tx = ws_tx.clone();
                    let audio_ws_tx = audio_ws_tx.clone();
                    let audio_routes = audio_routes.clone();
                    let auth = auth_secret.clone();
                    std::thread::Builder::new()
                        .name("http-handler".into())
                        .spawn(move || {
                            let _guard = guard;
                            let conn = match rustls::ServerConnection::new(tls) {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::warn!("ServerConnection::new failed: {e}");
                                    #[cfg(target_os = "windows")]
                                    crate::service_win::svc_log(&format!(
                                        "ServerConnection::new failed: {e}"
                                    ));
                                    return;
                                }
                            };
                            let mut stream = rustls::StreamOwned::new(conn, tcp_stream);

                            let _ = stream.sock.set_read_timeout(Some(KEEPALIVE_TIMEOUT));

                            for _req_num in 0..MAX_REQUESTS_PER_CONN {
                                match handle_http_rw(&mut stream, &auth) {
                                    Ok(HttpResult::WsUpgrade) => {
                                        spawn_ws_connection(stream, ws_tx, audio_routes);
                                        return;
                                    }
                                    Ok(HttpResult::WsUpgradeAudio(session)) => {
                                        spawn_audio_ws_connection(
                                            stream,
                                            audio_ws_tx,
                                            audio_routes,
                                            session,
                                        );
                                        return;
                                    }
                                    Ok(HttpResult::Done) => continue,
                                    Ok(HttpResult::Close) => break,
                                    Err(_) => break,
                                }
                            }
                        })
                        .ok();
                }
            });
        }

        Ok(Self {
            #[cfg(feature = "webrtc")]
            rtc_session,
            #[cfg(feature = "webrtc")]
            rtc_notify,
            ws_rx,
            audio_ws_rx,
        })
    }

    /// Accept: WebSocket (default). Blocks until a WS client connects.
    #[allow(dead_code)]
    pub fn accept_ws(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        let ws = self.ws_rx.recv().context("WS channel closed")?;
        tracing::info!("WebSocket client accepted");
        Ok((Box::new(ws.data_sender), Box::new(ws.data_receiver)))
    }

    /// Take the audio WS receiver (to pass to session runner).
    /// Can only be called once — subsequent calls return None.
    pub fn take_audio_ws_rx(&mut self) -> Option<mpsc::Receiver<WsSender>> {
        // Swap with a dummy channel
        let (_, dummy) = mpsc::channel();
        Some(std::mem::replace(&mut self.audio_ws_rx, dummy))
    }

    /// Accept: either WebRTC or WebSocket, whichever connects first.
    #[cfg(feature = "webrtc")]
    pub fn accept_any(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        loop {
            let _ = self
                .rtc_notify
                .recv_timeout(std::time::Duration::from_millis(50));
            if let Some((s, r)) = self
                .rtc_session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
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
    /// Request served; connection may be reused (keep-alive).
    Done,
    /// Client requested `Connection: close` or a non-keepalive response was sent.
    Close,
    /// WebSocket upgrade for main channel (video + control).
    WsUpgrade,
    /// WebSocket upgrade for audio-only channel.
    WsUpgradeAudio(Option<String>),
}

/// Check whether the client sent `Connection: close`.
fn wants_close(request: &str) -> bool {
    request.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("connection:") && lower.contains("close")
    })
}

#[cfg(feature = "webrtc")]
fn extract_content_length(request: &str) -> Option<usize> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

/// HTTP handler WITHOUT WebRTC (no POST /rtc).
#[cfg(not(feature = "webrtc"))]
fn handle_http_rw(
    stream: &mut (impl Read + Write),
    auth_secret: &Option<Vec<u8>>,
) -> Result<HttpResult> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        anyhow::bail!("client closed");
    }
    buf.truncate(n);
    let request = String::from_utf8_lossy(&buf);
    let raw_path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    let close = wants_close(&request);

    // WebSocket upgrade — detect audio-only path
    let request_lower = request.to_ascii_lowercase();
    if request_lower.contains("upgrade: websocket") {
        check_request_auth(stream, raw_path, auth_secret)?;
        send_ws_upgrade(stream, &request)?;
        if path.contains("audio") {
            return Ok(HttpResult::WsUpgradeAudio(
                extract_query_param(raw_path, "session").map(str::to_owned),
            ));
        }
        return Ok(HttpResult::WsUpgrade);
    }

    serve_static(stream, path, !close)?;
    if close {
        Ok(HttpResult::Close)
    } else {
        Ok(HttpResult::Done)
    }
}

/// HTTP handler WITH WebRTC (POST /rtc + static + WSS upgrade).
#[cfg(feature = "webrtc")]
fn handle_http_rw_rtc(
    stream: &mut (impl Read + Write),
    rtc_tx: &mpsc::Sender<RtcRequest>,
    candidate_addr: std::net::SocketAddr,
    auth_secret: &Option<Vec<u8>>,
) -> Result<HttpResult> {
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        anyhow::bail!("client closed");
    }
    buf.truncate(n);
    let request = String::from_utf8_lossy(&buf).into_owned();
    let method = request.split_whitespace().next().unwrap_or("GET");
    let raw_path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    let close = wants_close(&request);

    // WebSocket upgrade — detect audio-only path
    let request_lower = request.to_ascii_lowercase();
    if request_lower.contains("upgrade: websocket") {
        check_request_auth(stream, raw_path, auth_secret)?;
        send_ws_upgrade(stream, &request)?;
        if path.contains("audio") {
            return Ok(HttpResult::WsUpgradeAudio(
                extract_query_param(raw_path, "session").map(str::to_owned),
            ));
        }
        return Ok(HttpResult::WsUpgrade);
    }

    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(n);
    if method == "POST" {
        if let Some(content_len) = extract_content_length(&request) {
            let wanted = body_start + content_len;
            while buf.len() < wanted {
                let mut chunk = vec![0u8; (wanted - buf.len()).min(16 * 1024)];
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    anyhow::bail!(
                        "unexpected EOF while reading HTTP body: have {} of {} bytes",
                        buf.len().saturating_sub(body_start),
                        content_len
                    );
                }
                chunk.truncate(read);
                buf.extend_from_slice(&chunk);
            }
        }
    }

    match (method, path) {
        ("POST", "/rtc") => {
            check_request_auth(stream, raw_path, auth_secret)?;
            let body = &buf[body_start..];
            tracing::info!(body_len = body.len(), "POST /rtc received");
            let offer_json: serde_json::Value = serde_json::from_slice(body)?;
            let sdp_str = offer_json["sdp"].as_str().context("missing sdp")?;
            let rtc_mode = super::webrtc::RtcMode::from_offer_mode(
                offer_json["mode"].as_str().unwrap_or("datachannel_v1"),
            );
            tracing::info!(
                ?rtc_mode,
                sdp_len = sdp_str.len(),
                "POST /rtc parsed offer JSON"
            );

            let accepted = super::webrtc::accept_http_offer(candidate_addr, sdp_str, rtc_mode)?;
            tracing::info!("POST /rtc accepted offer");
            let answer_json = serde_json::json!({ "type": "answer", "sdp": accepted.answer_sdp });

            rtc_tx
                .send(accepted.session)
                .map_err(|_| anyhow::anyhow!("rtc channel closed"))?;
            tracing::info!("POST /rtc queued rtc for run loop");

            let resp_body = answer_json.to_string();
            let conn_header = if close { "close" } else { "keep-alive" };
            write!(stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: {conn_header}\r\n\r\n",
                resp_body.len()
            )?;
            stream.write_all(resp_body.as_bytes())?;
            stream.flush()?;
            tracing::info!(?rtc_mode, "SDP offer/answer exchanged via POST /rtc");
        }
        ("OPTIONS", "/rtc") => {
            let conn_header = if close { "close" } else { "keep-alive" };
            write!(stream,
                "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: {conn_header}\r\n\r\n"
            )?;
            stream.flush()?;
        }
        _ => {
            serve_static(stream, path, !close)?;
        }
    }

    if close {
        Ok(HttpResult::Close)
    } else {
        Ok(HttpResult::Done)
    }
}

fn send_ws_upgrade(stream: &mut (impl Read + Write), request: &str) -> Result<HttpResult> {
    let key = request
        .lines()
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

/// Serve a static file response with keep-alive or close semantics.
fn serve_static(stream: &mut (impl Read + Write), path: &str, keep_alive: bool) -> Result<()> {
    let (status, content_type, body): (&str, &str, &[u8]) = match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/phantom_web.js" => ("200 OK", "application/javascript", WASM_JS),
        "/phantom_web_bg.wasm" => ("200 OK", "application/wasm", WASM_BIN),
        _ => ("404 Not Found", "text/plain", b"404 Not Found"),
    };
    let conn_header = if keep_alive { "keep-alive" } else { "close" };
    write!(stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nCross-Origin-Opener-Policy: same-origin\r\nCross-Origin-Embedder-Policy: require-corp\r\nConnection: {conn_header}\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn spawn_ws_connection(
    stream: rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    ws_tx: mpsc::Sender<WsConnection>,
    audio_routes: AudioRoutes,
) {
    // Winlogon uses software H.264 and can produce a substantially larger
    // recovery keyframe than the steady-state NVENC stream. A 150 ms socket
    // timeout could tear down an otherwise healthy WAN connection halfway
    // through that keyframe, leaving the browser in a reconnect loop. Queue
    // pressure below remains the authoritative stale-client guard.
    let stream = match ws_poll_stream(stream, WS_SOCKET_WRITE_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "WebSocket socket configuration failed");
            return;
        }
    };
    let ws =
        tungstenite::WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
    tracing::info!("WebSocket client connected via HTTPS port");
    let (send_tx, send_rx) = mpsc::sync_channel(WS_VIDEO_SEND_QUEUE_DEPTH);
    let (recv_tx, recv_rx) = mpsc::channel();
    let recovery = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let io_recovery = recovery.clone();
    std::thread::spawn(move || ws_io_loop(ws, send_rx, recv_tx, io_recovery));
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let _ = ws_tx.send(WsConnection {
        data_sender: WsSender {
            tx: send_tx,
            dropping_video_until_keyframe: false,
            video_backlog_started_at: None,
            recovery,
            dropped,
            audio: Some(SessionAudio::new(audio_routes)),
        },
        data_receiver: WsReceiver { rx: recv_rx },
    });
}

/// Audio-only WebSocket — send only, no receive needed.
fn spawn_audio_ws_connection(
    stream: rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    audio_ws_tx: mpsc::Sender<WsSender>,
    audio_routes: AudioRoutes,
    session: Option<String>,
) {
    let stream = match ws_poll_stream(stream, Duration::from_millis(250)) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "Audio WebSocket socket configuration failed");
            return;
        }
    };
    let ws =
        tungstenite::WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
    tracing::info!("audio WebSocket connected");
    let (send_tx, send_rx) = mpsc::sync_channel(WS_AUDIO_SEND_QUEUE_DEPTH);
    if let Some(ref key) = session {
        if !audio_routes.attach(key, send_tx.clone()) {
            tracing::warn!("Audio WebSocket session is unknown, ended or already attached");
            return;
        }
        tracing::info!("Audio WebSocket paired with active WSS session");
    }
    let (recv_tx, _recv_rx) = mpsc::channel();
    let recovery = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let io_recovery = recovery.clone();
    std::thread::spawn(move || ws_io_loop(ws, send_rx, recv_tx, io_recovery));
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Paired audio is owned by its main sender, including in Windows service mode.
    if session.is_some() {
        return;
    }
    let _ = audio_ws_tx.send(WsSender {
        tx: send_tx,
        dropping_video_until_keyframe: false,
        video_backlog_started_at: None,
        recovery,
        dropped,
        audio: None,
    });
}

#[cfg(feature = "webrtc")]
fn get_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

// -- WebSocket IO loop --

/// The TLS/WebSocket stream has one owning I/O thread. Poll reads without a
/// socket receive timeout, then restore blocking mode before TLS writes.
/// This avoids Windows' short SO_RCVTIMEO path (which returned error 997 in
/// the canary soak) without treating an unexpected native error as success.
/// Writes keep their bounded blocking semantics: tungstenite may have already
/// accepted part of a frame, so retrying a whole send on WouldBlock is unsafe.
struct WsPollSocket(std::net::TcpStream);

impl Read for WsPollSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.set_nonblocking(true)?;
        let result = self.0.read(buf);
        self.0.set_nonblocking(false)?;
        result
    }
}

impl Write for WsPollSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        self.0.write_vectored(bufs)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn ws_poll_stream(
    stream: rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    write_timeout: Duration,
) -> std::io::Result<rustls::StreamOwned<rustls::ServerConnection, WsPollSocket>> {
    stream.sock.set_read_timeout(None)?;
    stream.sock.set_write_timeout(Some(write_timeout))?;
    stream.sock.set_nodelay(true)?;
    Ok(rustls::StreamOwned::new(
        stream.conn,
        WsPollSocket(stream.sock),
    ))
}

fn ws_io_loop<S: std::io::Read + std::io::Write>(
    mut ws: WebSocket<S>,
    send_rx: mpsc::Receiver<Vec<u8>>,
    recv_tx: mpsc::Sender<Vec<u8>>,
    recovery: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        // Drain a bounded number of pending outgoing messages. If we drain the
        // whole queue under video load, WSS reads are starved and input/pong
        // packets sit behind outbound frames on the same thread.
        let mut writes = 0usize;
        // If the sender end was dropped
        // (session ended, replaced, cancelled by watcher, etc.), exit so
        // the TCP stream drops, the client's onclose fires, and the web
        // client can auto-reconnect. Without this, the io loop idle-loops
        // on ws.read() forever and the client sees a hung connection.
        while writes < WS_MAX_WRITES_PER_READ {
            match send_rx.try_recv() {
                Ok(data) => {
                    writes += 1;
                    let bytes = data.len();
                    if let Err(error) = ws.send(tungstenite::Message::Binary(data)) {
                        tracing::warn!(%error, bytes, "WebSocket write failed; closing connection");
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // A transient full queue can lose a delta while the TCP
                    // writer subsequently catches up. Ask the session for a
                    // fresh keyframe only after draining; otherwise waiting
                    // for its periodic keyframe can hit the backlog deadline
                    // even though the socket has recovered. Coalesce requests
                    // and leave the queue/write/deadline bounds unchanged.
                    if recovery.swap(false, std::sync::atomic::Ordering::AcqRel) {
                        let request = bincode::serialize(&Message::RequestKeyframe)
                            .expect("unit protocol message serializes");
                        if recv_tx.send(request).is_err() {
                            return;
                        }
                        tracing::info!("WSS send queue drained; requesting recovery keyframe");
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::debug!("WebSocket sender dropped; closing connection");
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
            }
        }
        match ws.read() {
            Ok(tungstenite::Message::Binary(data)) => {
                if recv_tx.send(data).is_err() {
                    return;
                }
            }
            Ok(tungstenite::Message::Close(frame)) => {
                tracing::debug!(?frame, "WebSocket peer closed connection");
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                if writes == 0 {
                    std::thread::sleep(WS_READ_POLL_INTERVAL);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "WebSocket read failed; closing connection");
                return;
            }
        }
    }
}

/// Bound on the main WebSocket send queue. Keep this intentionally small:
/// stale video frames are worse than dropped frames for an interactive desktop.
const WS_VIDEO_SEND_QUEUE_DEPTH: usize = 8;

/// Audio uses its own WSS channel and benefits from a deeper jitter buffer.
const WS_AUDIO_SEND_QUEUE_DEPTH: usize = 50;

// Sleep only when neither direction made progress. Outbound media is never
// paced by an idle peer's read timeout. Keep queue bounds and read fairness.
const WS_READ_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Interleave writes with reads so client input/pong is not starved by video.
const WS_MAX_WRITES_PER_READ: usize = 4;

/// Permit a recovery keyframe to cross a slower WAN link before declaring the
/// socket unhealthy. The sender-side backlog timer below still bounds stale
/// video and closes connections that cannot make sustained progress.
const WS_SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A sustained blocked TCP writer is not recoverable in-place. Reconnect before
/// browser/kernel buffers turn old desktop frames into visible fast-forward.
const WS_VIDEO_BACKLOG_CLOSE_AFTER: Duration = Duration::from_secs(2);

pub struct WsSender {
    audio: Option<SessionAudio>,
    tx: mpsc::SyncSender<Vec<u8>>,
    dropping_video_until_keyframe: bool,
    video_backlog_started_at: Option<Instant>,
    /// Set by the producer on video loss; consumed by the I/O owner only
    /// after the outgoing queue drains, when a recovery keyframe can fit.
    recovery: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Counts payloads we couldn't push because the IO loop was too far
    /// behind (client TCP receive buffer full → IO loop blocked on write
    /// → channel full). Logged occasionally; never panics.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl WsSender {
    /// How many outgoing messages have been dropped since this sender was
    /// created (current process lifetime).
    #[allow(dead_code)]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl MessageSender for WsSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        if let (Some(audio), Message::Hello { session_token, .. }) = (&mut self.audio, msg) {
            audio.register(session_token);
        }
        let video_keyframe = match msg {
            Message::VideoFrame { frame, .. } => Some(frame.is_keyframe),
            _ => None,
        };
        if self.dropping_video_until_keyframe && video_keyframe == Some(false) {
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self
                .video_backlog_started_at
                .is_some_and(|since| since.elapsed() >= WS_VIDEO_BACKLOG_CLOSE_AFTER)
            {
                return Err(anyhow::anyhow!("ws video backlog exceeded"));
            }
            return Ok(());
        }

        let mut payload = bincode::serialize(msg).context("serialize")?;
        if let (Some(audio), Message::AudioFrame { .. }) = (&self.audio, msg) {
            match audio.send(payload) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(_)) => {
                    self.dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                Err(mpsc::TrySendError::Disconnected(data)) => payload = data,
            }
        }
        match self.tx.try_send(payload) {
            Ok(()) => {
                if video_keyframe.is_some() {
                    self.dropping_video_until_keyframe = false;
                    self.video_backlog_started_at = None;
                    self.recovery
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Full(_)) => {
                // IO loop is far behind — client's TCP receive side is
                // stalled (laptop asleep, network frozen). Drop this
                // message so the queue doesn't grow without bound;
                // periodic keyframe + ABR will recover the stream once
                // the client catches up.
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if video_keyframe.is_some() {
                    self.dropping_video_until_keyframe = true;
                    self.recovery
                        .store(true, std::sync::atomic::Ordering::Release);
                    let since = self
                        .video_backlog_started_at
                        .get_or_insert_with(Instant::now);
                    if since.elapsed() >= WS_VIDEO_BACKLOG_CLOSE_AFTER {
                        return Err(anyhow::anyhow!("ws video backlog exceeded"));
                    }
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(anyhow::anyhow!("ws closed")),
        }
    }
}

pub struct WsReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
}
impl MessageReceiver for WsReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        let data = self.rx.recv().context("ws closed")?;
        bincode::deserialize(&data).context("deserialize")
    }

    fn recv_msg_within(&mut self, timeout: std::time::Duration) -> Result<Option<Message>> {
        match self.rx.recv_timeout(timeout) {
            Ok(data) => bincode::deserialize(&data).context("deserialize").map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("ws closed")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_core::encode::{EncodedFrame, VideoCodec};

    #[test]
    fn paired_audio_bypasses_a_full_video_queue_and_falls_back_after_close() {
        let routes = AudioRoutes::default();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut sender = WsSender {
            audio: Some(SessionAudio::new(routes.clone())),
            tx,
            dropping_video_until_keyframe: false,
            video_backlog_started_at: None,
            recovery: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        sender
            .send_msg(&Message::Hello {
                width: 16,
                height: 16,
                format: phantom_core::frame::PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: true,
                video_codec: VideoCodec::H264,
                session_token: vec![9; 32],
            })
            .unwrap();
        assert!(matches!(
            bincode::deserialize::<Message>(&rx.recv().unwrap()).unwrap(),
            Message::Hello { .. }
        ));
        let (audio_tx, audio_rx) = mpsc::sync_channel(2);
        assert!(routes.attach(&"09".repeat(32), audio_tx));
        sender.send_msg(&video(1, true)).unwrap(); // main queue stays full
        let audio = Message::AudioFrame {
            codec: phantom_core::protocol::AudioCodec::Opus,
            sample_rate: 48000,
            channels: 2,
            data: vec![5, 6],
        };
        sender.send_msg(&audio).unwrap();
        assert!(
            matches!(bincode::deserialize::<Message>(&audio_rx.recv().unwrap()).unwrap(), Message::AudioFrame { data, .. } if data == [5, 6])
        );
        assert_eq!(sender.dropped_count(), 0);
        assert!(matches!(
            bincode::deserialize::<Message>(&rx.recv().unwrap()).unwrap(),
            Message::VideoFrame { sequence: 1, .. }
        ));
        drop(audio_rx);
        sender.send_msg(&audio).unwrap();
        assert!(
            matches!(bincode::deserialize::<Message>(&rx.recv().unwrap()).unwrap(), Message::AudioFrame { data, .. } if data == [5, 6])
        );
    }

    #[test]
    fn poll_socket_preserves_data_and_restores_blocking_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let mut socket = WsPollSocket(socket);
        let mut byte = [0];
        assert_eq!(
            socket.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        socket.write_all(b"request").unwrap();
        let sender = std::thread::spawn(move || {
            let mut request = [0; 7];
            peer.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"request");
            std::thread::sleep(Duration::from_millis(20));
            peer.write_all(b"reply").unwrap();
        });
        // Bypass the polling wrapper to verify its previous empty read left
        // the socket blocking, with the peer's reply still intact.
        socket
            .0
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reply = [0; 5];
        socket.0.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"reply");
        sender.join().unwrap();
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    }

    fn video(sequence: u64, is_keyframe: bool) -> Message {
        Message::VideoFrame {
            sequence,
            frame: Box::new(EncodedFrame {
                codec: VideoCodec::H264,
                data: vec![sequence as u8],
                is_keyframe,
            }),
        }
    }

    #[test]
    fn queue_loss_drops_deltas_until_next_keyframe() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut sender = WsSender {
            audio: None,
            tx,
            dropping_video_until_keyframe: false,
            video_backlog_started_at: None,
            recovery: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        sender.send_msg(&video(1, true)).unwrap();
        sender.send_msg(&video(2, false)).unwrap();
        assert!(sender.dropping_video_until_keyframe);

        let _ = rx.recv().unwrap();
        sender.send_msg(&video(3, false)).unwrap();
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        sender.send_msg(&video(4, true)).unwrap();
        assert!(!sender.dropping_video_until_keyframe);
        let decoded: Message = bincode::deserialize(&rx.recv().unwrap()).unwrap();
        assert!(matches!(
            decoded,
            Message::VideoFrame {
                sequence: 4,
                frame
            } if frame.is_keyframe
        ));
    }

    #[test]
    fn drained_queue_requests_one_keyframe_and_preserves_frame_order() {
        use std::sync::{atomic::AtomicBool, Arc, Mutex};

        struct PausedWriter {
            entered: Option<mpsc::Sender<()>>,
            resume: mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
        }
        impl Read for PausedWriter {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::WouldBlock.into())
            }
        }
        impl Write for PausedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.resume.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let socket = PausedWriter {
            entered: Some(entered_tx),
            resume: resume_rx,
            bytes: bytes.clone(),
        };
        let ws = WebSocket::from_raw_socket(socket, tungstenite::protocol::Role::Server, None);
        let (tx, rx) = mpsc::sync_channel(1);
        let (feedback_tx, feedback_rx) = mpsc::channel();
        let recovery = Arc::new(AtomicBool::new(false));
        let mut sender = WsSender {
            audio: None,
            tx,
            dropping_video_until_keyframe: false,
            video_backlog_started_at: None,
            recovery: recovery.clone(),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let io = std::thread::spawn(move || ws_io_loop(ws, rx, feedback_tx, recovery));

        sender.send_msg(&video(1, true)).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        sender.send_msg(&video(2, false)).unwrap(); // buffered behind paused writer
        sender.send_msg(&video(3, false)).unwrap(); // full: must discard later deltas
        assert!(sender.dropping_video_until_keyframe);
        assert!(matches!(
            feedback_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        resume_tx.send(()).unwrap();
        let feedback = feedback_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            bincode::deserialize::<Message>(&feedback).unwrap(),
            Message::RequestKeyframe
        ));
        sender.send_msg(&video(4, false)).unwrap(); // still invalid until recovery IDR
        sender.send_msg(&video(5, true)).unwrap();
        assert!(!sender.dropping_video_until_keyframe);
        assert_eq!(sender.dropped_count(), 2);
        drop(sender);
        io.join().unwrap();
        assert!(matches!(
            feedback_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));

        let wire = std::io::Cursor::new(bytes.lock().unwrap().clone());
        let mut reader =
            WebSocket::from_raw_socket(wire, tungstenite::protocol::Role::Client, None);
        for expected in [1, 2, 5] {
            let data = reader.read().unwrap().into_data();
            assert!(matches!(
                bincode::deserialize::<Message>(&data).unwrap(),
                Message::VideoFrame { sequence, .. } if sequence == expected
            ));
        }
        assert!(matches!(
            reader.read().unwrap(),
            tungstenite::Message::Close(_)
        ));
    }

    #[test]
    fn persistent_full_queue_still_closes_at_backlog_deadline() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut sender = WsSender {
            audio: None,
            tx,
            dropping_video_until_keyframe: false,
            video_backlog_started_at: None,
            recovery: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        sender.send_msg(&video(1, true)).unwrap();
        sender.send_msg(&video(2, false)).unwrap();
        sender.video_backlog_started_at = Some(Instant::now() - WS_VIDEO_BACKLOG_CLOSE_AFTER);
        assert!(sender
            .send_msg(&video(3, false))
            .unwrap_err()
            .to_string()
            .contains("backlog exceeded"));
    }
}
