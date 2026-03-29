use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use tungstenite::WebSocket;

/// Embedded static files for the web client.
const INDEX_HTML: &str = include_str!("../web/index.html");
const WASM_JS: &[u8] = include_bytes!("../../web/pkg/phantom_web.js");
const WASM_BIN: &[u8] = include_bytes!("../../web/pkg/phantom_web_bg.wasm");

/// Start the HTTP static file server (background thread).
pub fn start_http_server(port: u16) -> Result<()> {
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).context("bind HTTP server")?;
    tracing::info!(addr = %addr, "HTTP static file server");

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || { let _ = serve_http(stream); });
        }
    });

    Ok(())
}

pub struct WsServerTransport {
    /// Receives fully upgraded WebSocket connections.
    conn_rx: mpsc::Receiver<(WsSender, WsReceiver)>,
}

impl WsServerTransport {
    pub fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).context("bind WebSocket")?;
        tracing::info!(addr, "WebSocket server listening");

        let (conn_tx, conn_rx) = mpsc::channel();

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_nodelay(true);
                let peer = stream.peer_addr().ok();

                match tungstenite::accept(stream) {
                    Ok(ws) => {
                        tracing::info!(?peer, "WebSocket connected");

                        // Split WebSocket into sender/receiver using channels.
                        // One thread owns the WebSocket, two channels bridge send/recv.
                        let (send_tx, send_rx) = mpsc::channel::<Vec<u8>>();
                        let (recv_tx, recv_rx) = mpsc::channel::<Vec<u8>>();

                        // IO thread: owns the WebSocket, reads and writes
                        std::thread::spawn(move || {
                            ws_io_loop(ws, send_rx, recv_tx);
                        });

                        let _ = conn_tx.send((
                            WsSender { tx: send_tx },
                            WsReceiver { rx: recv_rx },
                        ));
                    }
                    Err(e) => {
                        tracing::debug!(?peer, "WebSocket handshake failed: {e}");
                    }
                }
            }
        });

        Ok(Self { conn_rx })
    }

    pub fn accept(&self) -> Result<(WsSender, WsReceiver)> {
        self.conn_rx.recv().context("WebSocket channel closed")
    }
}

/// Single thread that owns the WebSocket and handles both read and write.
fn ws_io_loop(
    mut ws: WebSocket<TcpStream>,
    send_rx: mpsc::Receiver<Vec<u8>>,
    recv_tx: mpsc::Sender<Vec<u8>>,
) {
    if let Ok(stream) = ws.get_ref().try_clone() {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(5)));
    }

    let mut recv_count: u64 = 0;
    let mut send_count: u64 = 0;

    loop {
        // Send outgoing
        while let Ok(data) = send_rx.try_recv() {
            send_count += 1;
            if send_count <= 5 {
                tracing::debug!(send_count, bytes = data.len(), "ws send");
            }
            if ws.send(tungstenite::Message::Binary(data)).is_err() {
                tracing::debug!("ws send failed, closing");
                return;
            }
        }

        // Read incoming
        match ws.read() {
            Ok(tungstenite::Message::Binary(data)) => {
                recv_count += 1;
                if recv_count <= 5 {
                    tracing::info!(recv_count, bytes = data.len(), "ws recv from client");
                }
                if recv_tx.send(data).is_err() {
                    return;
                }
            }
            Ok(tungstenite::Message::Close(_)) => {
                tracing::debug!("ws close received");
                return;
            }
            Ok(msg) => {
                tracing::debug!("ws other: {:?}", msg);
            }
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::debug!("ws read error: {e}");
                return;
            }
        }
    }
}

fn serve_http(mut stream: TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Consume remaining headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() { break; }
    }

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");

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
    Ok(())
}

pub struct WsSender {
    tx: mpsc::Sender<Vec<u8>>,
}

impl MessageSender for WsSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
        self.tx.send(payload).map_err(|_| anyhow::anyhow!("ws send channel closed"))
    }
}

pub struct WsReceiver {
    rx: mpsc::Receiver<Vec<u8>>,
}

impl MessageReceiver for WsReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        let data = self.rx.recv().context("ws recv channel closed")?;
        bincode::deserialize(&data).context("deserialize")
    }
}
