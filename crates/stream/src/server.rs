//! StreamServer — serves encoded frames to browsers via WebSocket.
//!
//! Provides an all-in-one API: HTTP viewer + WebSocket H.264/AV1 streaming.

use crate::pipeline::{StreamConfig, StreamPipeline};
use crate::source::StreamSource;
use anyhow::Result;
use phantom_core::encode::VideoCodec;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Built-in HTML viewer page.
const VIEWER_HTML: &str = include_str!("viewer.html");

/// Encoded frame ready for WebSocket delivery.
struct SharedState {
    /// Latest encoded video frame.
    latest_frame: Option<Vec<u8>>,
    /// Frame sequence number.
    frame_num: u64,
    /// Video codec (for client initialization).
    _codec: VideoCodec,
    /// Resolution.
    _width: u32,
    _height: u32,
    /// Number of connected clients.
    client_count: usize,
}

/// All-in-one streaming server.
///
/// ```rust,ignore
/// let server = StreamServer::new(my_source, 8080)?;
/// server.run(); // blocks
/// ```
pub struct StreamServer {
    source: Box<dyn StreamSource>,
    config: StreamConfig,
    http_port: u16,
    ws_port: u16,
}

impl StreamServer {
    /// Create a new server.
    /// - `source`: anything implementing `StreamSource`
    /// - `port`: HTTP viewer port (WebSocket will be port+1)
    pub fn new(source: impl StreamSource + 'static, port: u16) -> Result<Self> {
        Ok(Self {
            source: Box::new(source),
            config: StreamConfig::default(),
            http_port: port,
            ws_port: port + 1,
        })
    }

    /// Set streaming configuration.
    pub fn with_config(mut self, config: StreamConfig) -> Self {
        self.config = config;
        self
    }

    /// Run the server (blocks).
    ///
    /// 1. Starts HTTP server for the viewer page
    /// 2. Starts WebSocket server for video frames
    /// 3. Runs the encode loop: source → NVENC → WebSocket
    pub fn run(mut self) -> Result<()> {
        let (w, h) = self.source.resolution();
        let state = Arc::new(Mutex::new(SharedState {
            latest_frame: None,
            frame_num: 0,
            _codec: self.config.codec,
            _width: w,
            _height: h,
            client_count: 0,
        }));

        // Start HTTP server
        start_http(self.http_port);

        // Start WebSocket server
        let ws_state = Arc::clone(&state);
        start_ws(self.ws_port, ws_state);

        println!("  🌐 Viewer: http://localhost:{}", self.http_port);
        println!("  📡 Stream: ws://localhost:{}", self.ws_port);
        println!("  Resolution: {}×{}", w, h);
        println!(
            "  Codec: {:?}, Bitrate: {}kbps, FPS: {}",
            self.config.codec, self.config.bitrate_kbps, self.config.fps
        );

        // Encode loop
        let mut pipeline = StreamPipeline::new(self.config);

        loop {
            match pipeline.process_frame(self.source.as_mut()) {
                Ok(Some(encoded)) => {
                    let mut s = state.lock().unwrap();
                    s.latest_frame = Some(encoded.data);
                    s.frame_num += 1;
                }
                Ok(None) => {
                    // No frame ready, sleep briefly
                    thread::sleep(Duration::from_micros(500));
                }
                Err(e) => {
                    tracing::error!("encode error: {e}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn start_http(port: u16) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).expect("Failed to bind HTTP server");

    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    VIEWER_HTML.len(),
                    VIEWER_HTML
                );
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
}

fn start_ws(port: u16, state: Arc<Mutex<SharedState>>) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).expect("Failed to bind WebSocket server");

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let st = Arc::clone(&state);
            thread::spawn(move || {
                let mut ws = match tungstenite::accept(stream) {
                    Ok(ws) => ws,
                    Err(_) => return,
                };

                // Track client connection
                {
                    let mut s = st.lock().unwrap();
                    s.client_count += 1;
                }

                let mut last_frame = 0u64;

                loop {
                    let data = {
                        let s = st.lock().unwrap();
                        if s.frame_num > last_frame {
                            last_frame = s.frame_num;
                            s.latest_frame.clone()
                        } else {
                            None
                        }
                    };

                    if let Some(data) = data {
                        if ws.send(tungstenite::Message::Binary(data.into())).is_err() {
                            break;
                        }
                    }

                    thread::sleep(Duration::from_millis(4)); // ~250fps max poll rate
                }

                // Track client disconnection
                {
                    let mut s = st.lock().unwrap();
                    s.client_count = s.client_count.saturating_sub(1);
                }
            });
        }
    });
}
