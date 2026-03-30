use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use str0m::{Candidate, Rtc};
use tungstenite::WebSocket;

/// Embedded static files for the web client.
const INDEX_HTML: &str = include_str!("../web/index.html");
const WASM_JS: &[u8] = include_bytes!("../../web/pkg/phantom_web.js");
const WASM_BIN: &[u8] = include_bytes!("../../web/pkg/phantom_web_bg.wasm");

/// Combined web server: HTTP static files + WebSocket fallback + WebRTC via POST /rtc.
pub struct WebServerTransport {
    rtc_rx: mpsc::Receiver<Rtc>,
    ws_rx: mpsc::Receiver<WsConnection>,
    pub udp_socket: UdpSocket,
    /// The address str0m told the browser to connect to (must match Receive.destination)
    pub candidate_addr: std::net::SocketAddr,
}

pub struct WsConnection {
    pub data_sender: WsSender,
    pub data_receiver: WsReceiver,
}

impl WebServerTransport {
    pub fn start(http_port: u16, ws_port: u16, udp_port: u16) -> Result<Self> {
        // UDP socket for WebRTC
        let udp_socket = UdpSocket::bind(format!("0.0.0.0:{udp_port}"))
            .context("bind UDP")?;
        let udp_addr = udp_socket.local_addr()?;

        // ICE candidate IP: use PHANTOM_HOST env var, or detect, or fallback to 127.0.0.1
        let host_ip: std::net::IpAddr = std::env::var("PHANTOM_HOST")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| get_local_ip().unwrap_or([127, 0, 0, 1].into()));
        let candidate_addr = std::net::SocketAddr::new(host_ip, udp_addr.port());
        tracing::info!(%candidate_addr, "WebRTC ICE candidate address");

        // Channel for Rtc instances from POST /rtc
        let (rtc_tx, rtc_rx) = mpsc::channel::<Rtc>();
        // Channel for WS fallback connections
        let (ws_tx, ws_rx) = mpsc::channel::<WsConnection>();

        // HTTP server thread (serves static files + POST /rtc)
        let http_addr = format!("0.0.0.0:{http_port}");
        let http_listener = TcpListener::bind(&http_addr).context("bind HTTP")?;
        tracing::info!(addr = %http_addr, "HTTP server (static + POST /rtc)");

        std::thread::spawn(move || {
            for stream in http_listener.incoming().flatten() {
                let rtc_tx = rtc_tx.clone();
                let candidate_addr = candidate_addr;
                std::thread::spawn(move || {
                    let _ = handle_http(stream, rtc_tx, candidate_addr);
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

        Ok(Self { rtc_rx, ws_rx, udp_socket, candidate_addr })
    }

    /// Accept: WebRTC only (via POST /rtc). Blocks until a WebRTC client connects.
    pub fn accept_webrtc(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        loop {
            if let Ok(rtc) = self.rtc_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                tracing::info!("WebRTC client accepted via POST /rtc");
                // Pass candidate_addr as destination for str0m Receive matching
                let (s, r) = super::transport_webrtc::run_rtc(rtc, &self.udp_socket, self.candidate_addr)?;
                return Ok((Box::new(s), Box::new(r)));
            }
        }
    }

    /// Accept: WebSocket only (fallback mode).
    pub fn accept_ws(&self) -> Result<(Box<dyn MessageSender>, Box<dyn MessageReceiver>)> {
        let ws = self.ws_rx.recv().context("WS channel closed")?;
        tracing::info!("WebSocket client accepted");
        Ok((Box::new(ws.data_sender), Box::new(ws.data_receiver)))
    }
}

fn handle_http(
    mut stream: TcpStream,
    rtc_tx: mpsc::Sender<Rtc>,
    candidate_addr: std::net::SocketAddr,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let method = request_line.split_whitespace().next().unwrap_or("GET");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");

    // Read headers
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() { break; }
        if let Some(val) = line.strip_prefix("Content-Length:").or(line.strip_prefix("content-length:")) {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    match (method, path) {
        ("POST", "/rtc") => {
            // Read SDP offer body
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body)?;
            let offer_json: serde_json::Value = serde_json::from_slice(&body)?;
            let sdp_str = offer_json["sdp"].as_str().context("missing sdp")?;

            // Create Rtc, accept offer, return answer
            let mut rtc = Rtc::builder().build();
            let candidate = Candidate::host(candidate_addr, "udp")
                .context("host candidate")?;
            rtc.add_local_candidate(candidate);

            let offer = str0m::change::SdpOffer::from_sdp_string(sdp_str)
                .context("parse SDP")?;
            let answer = rtc.sdp_api().accept_offer(offer)
                .context("accept offer")?;

            let answer_json = serde_json::json!({
                "type": "answer",
                "sdp": answer.to_sdp_string(),
            });

            // Send Rtc to main thread for the IO loop
            rtc_tx.send(rtc).map_err(|_| anyhow::anyhow!("rtc channel closed"))?;

            // Respond with SDP answer
            let resp_body = answer_json.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                resp_body.len()
            )?;
            stream.write_all(resp_body.as_bytes())?;
            stream.flush()?;
            tracing::info!("SDP offer/answer exchanged via POST /rtc");
        }
        ("OPTIONS", "/rtc") => {
            // CORS preflight
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n"
            )?;
            stream.flush()?;
        }
        _ => {
            // Serve static files
            let (status, content_type, body): (&str, &str, &[u8]) = match path {
                "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
                "/phantom_web.js" => ("200 OK", "application/javascript", WASM_JS),
                "/phantom_web_bg.wasm" => ("200 OK", "application/wasm", WASM_BIN),
                _ => ("404 Not Found", "text/plain", b"404 Not Found"),
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
            stream.flush()?;
        }
    }
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
