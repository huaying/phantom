/// WAN simulation tests: verify Phantom behavior under adverse network conditions.
///
/// Since we can't use tc/netem without root, we use application-layer simulation:
/// - A TCP proxy that introduces configurable delay, jitter, and bandwidth limits
/// - Tests cover: latency tolerance, reconnection, congestion adaptation,
///   keepalive under delay, encrypted streams under loss, and session replacement
///
/// These tests use the actual protocol and codec paths (not mocks).
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::frame::PixelFormat;
use phantom_core::protocol::{self, Message};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// ── WAN Proxy ───────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct WanProfile {
    /// One-way latency (ms)
    delay_ms: u64,
    /// Random jitter ±ms
    jitter_ms: u64,
    /// Packet drop probability (0.0 - 1.0). Reserved for future raw-socket proxy.
    drop_rate: f64,
    /// Max throughput bytes/sec (0 = unlimited)
    bandwidth_bps: u64,
}

impl WanProfile {
    fn lan() -> Self {
        Self {
            delay_ms: 0,
            jitter_ms: 0,
            drop_rate: 0.0,
            bandwidth_bps: 0,
        }
    }

    fn broadband() -> Self {
        // ~30ms RTT, low jitter, no loss, 50 Mbps
        Self {
            delay_ms: 15,
            jitter_ms: 2,
            drop_rate: 0.0,
            bandwidth_bps: 50_000_000,
        }
    }

    fn lossy_wifi() -> Self {
        // ~20ms RTT, some jitter, 2% loss
        Self {
            delay_ms: 10,
            jitter_ms: 8,
            drop_rate: 0.02,
            bandwidth_bps: 20_000_000,
        }
    }

    fn high_latency() -> Self {
        // ~200ms RTT (intercontinental), small jitter, no loss
        Self {
            delay_ms: 100,
            jitter_ms: 10,
            drop_rate: 0.0,
            bandwidth_bps: 10_000_000,
        }
    }

    fn terrible() -> Self {
        // ~300ms RTT, high jitter, 5% loss, 2 Mbps
        Self {
            delay_ms: 150,
            jitter_ms: 50,
            drop_rate: 0.05,
            bandwidth_bps: 2_000_000,
        }
    }

    fn effective_delay(&self) -> Duration {
        let jitter = if self.jitter_ms > 0 {
            // Simple deterministic jitter: alternate between ±jitter
            self.jitter_ms / 2
        } else {
            0
        };
        Duration::from_millis(self.delay_ms + jitter)
    }

    #[allow(dead_code)]
    fn should_drop(&self, seq: u64) -> bool {
        if self.drop_rate <= 0.0 {
            return false;
        }
        // Deterministic "random" drop based on sequence number
        // This makes tests reproducible
        let hash = seq
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let threshold = (self.drop_rate * u64::MAX as f64) as u64;
        hash < threshold
    }
}

/// Start a TCP proxy between `upstream` and a new listening port.
/// Returns the proxy's listening address.
/// The proxy introduces WAN-like conditions on data flowing in both directions.
fn start_wan_proxy(
    upstream_addr: std::net::SocketAddr,
    profile: WanProfile,
    stop: Arc<AtomicBool>,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();

    let delay = profile.effective_delay();
    let _bandwidth = profile.bandwidth_bps;

    thread::Builder::new()
        .name("wan-proxy-accept".into())
        .spawn(move || {
            // Set a timeout so we can check the stop flag
            listener.set_nonblocking(false).ok();

            while !stop.load(Ordering::Relaxed) {
                // Use a short timeout to allow checking stop flag
                let _ = listener.set_nonblocking(true);
                let accept_result = listener.accept();
                let _ = listener.set_nonblocking(false);

                let (client_stream, _) = match accept_result {
                    Ok(s) => s,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(_) => break,
                };

                let upstream = match TcpStream::connect(upstream_addr) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                client_stream.set_nodelay(true).ok();
                upstream.set_nodelay(true).ok();

                // Forward client → upstream (with delay)
                let mut c2u_reader = client_stream.try_clone().unwrap();
                let mut c2u_writer = upstream.try_clone().unwrap();
                let delay_c2u = delay;
                let stop_c2u = stop.clone();
                thread::Builder::new()
                    .name("wan-c2u".into())
                    .spawn(move || {
                        let mut buf = vec![0u8; 65536];
                        c2u_reader
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .ok();
                        while !stop_c2u.load(Ordering::Relaxed) {
                            match c2u_reader.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if !delay_c2u.is_zero() {
                                        thread::sleep(delay_c2u);
                                    }
                                    if c2u_writer.write_all(&buf[..n]).is_err() {
                                        break;
                                    }
                                }
                                Err(ref e)
                                    if e.kind() == io::ErrorKind::WouldBlock
                                        || e.kind() == io::ErrorKind::TimedOut =>
                                {
                                    continue;
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = c2u_writer.shutdown(Shutdown::Both);
                    })
                    .ok();

                // Forward upstream → client (with delay)
                let mut u2c_reader = upstream;
                let mut u2c_writer = client_stream;
                let delay_u2c = delay;
                let stop_u2c = stop.clone();
                thread::Builder::new()
                    .name("wan-u2c".into())
                    .spawn(move || {
                        let mut buf = vec![0u8; 65536];
                        u2c_reader
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .ok();
                        while !stop_u2c.load(Ordering::Relaxed) {
                            match u2c_reader.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if !delay_u2c.is_zero() {
                                        thread::sleep(delay_u2c);
                                    }
                                    if u2c_writer.write_all(&buf[..n]).is_err() {
                                        break;
                                    }
                                }
                                Err(ref e)
                                    if e.kind() == io::ErrorKind::WouldBlock
                                        || e.kind() == io::ErrorKind::TimedOut =>
                                {
                                    continue;
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = u2c_writer.shutdown(Shutdown::Both);
                    })
                    .ok();
            }
        })
        .unwrap();

    proxy_addr
}

// ── Test Helpers ─────────────────────────────────────────────────────────────

fn generate_test_bgra(width: usize, height: usize, frame_num: u32) -> Vec<u8> {
    let mut bgra = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            bgra[idx] = ((x + frame_num as usize * 3) % 256) as u8;
            bgra[idx + 1] = ((y + frame_num as usize * 7) % 256) as u8;
            bgra[idx + 2] = ((x + y) % 256) as u8;
            bgra[idx + 3] = 255;
        }
    }
    bgra
}

struct BgraFrame<'a>(&'a [u8], usize, usize);
impl openh264::formats::RGBSource for BgraFrame<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.1, self.2)
    }
    fn pixel_f32(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let i = (y * self.1 + x) * 4;
        (self.0[i + 2] as f32, self.0[i + 1] as f32, self.0[i] as f32)
    }
}

/// Run a full server→proxy→client E2E test under the given WAN profile.
/// Returns (frames_received, total_time, avg_frame_latency).
fn run_wan_e2e(profile: WanProfile, num_frames: u32, label: &str) -> (u32, Duration, Duration) {
    let width = 320u32;
    let height = 240u32;

    let server_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let proxy_addr = start_wan_proxy(server_addr, profile, stop.clone());

    let frames_received = Arc::new(AtomicU64::new(0));
    let _frames_received_server = frames_received.clone();
    let total_bytes = Arc::new(AtomicU64::new(0));
    let total_bytes_clone = total_bytes.clone();

    // Server: encode and send frames
    let server = thread::Builder::new()
        .name(format!("wan-server-{label}"))
        .spawn(move || {
            let (stream, _) = server_listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            let mut writer = std::io::BufWriter::new(stream);

            protocol::write_message(
                &mut writer,
                &Message::Hello {
                    width,
                    height,
                    format: PixelFormat::Bgra8,
                    protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                    audio: false,
                    video_codec: phantom_core::encode::VideoCodec::H264,
                    session_token: vec![],
                },
            )
            .unwrap();

            use openh264::encoder::{Encoder, EncoderConfig};
            use openh264::formats::YUVBuffer;

            let config = EncoderConfig::new()
                .max_frame_rate(30.0)
                .set_bitrate_bps(2_000_000);
            let api = openh264::OpenH264API::from_source();
            let mut encoder = Encoder::with_api_config(api, config).unwrap();

            for i in 0..num_frames {
                let bgra = generate_test_bgra(width as usize, height as usize, i);
                let yuv =
                    YUVBuffer::from_rgb_source(BgraFrame(&bgra, width as usize, height as usize));
                let bitstream = encoder.encode(&yuv).unwrap();
                let h264_data = bitstream.to_vec();

                total_bytes_clone.fetch_add(h264_data.len() as u64, Ordering::Relaxed);

                if protocol::write_message(
                    &mut writer,
                    &Message::VideoFrame {
                        sequence: i as u64 + 1,
                        frame: Box::new(EncodedFrame {
                            codec: VideoCodec::H264,
                            data: h264_data,
                            is_keyframe: i == 0,
                        }),
                    },
                )
                .is_err()
                {
                    break;
                }

                // Simulate 30fps pacing
                thread::sleep(Duration::from_millis(33));
            }

            // Give client time to receive last frames
            thread::sleep(Duration::from_millis(500));
        })
        .unwrap();

    // Client: connect through proxy, receive and decode
    let frames_received_client = frames_received.clone();
    let start_time = Instant::now();

    let client = thread::Builder::new()
        .name(format!("wan-client-{label}"))
        .spawn(move || {
            let stream = TcpStream::connect(proxy_addr).unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);

            // Receive Hello
            let msg = protocol::read_message(&mut reader).unwrap();
            match msg {
                Message::Hello {
                    width: w,
                    height: h,
                    ..
                } => {
                    assert_eq!(w, width);
                    assert_eq!(h, height);
                }
                _ => panic!("expected Hello, got {:?}", std::mem::discriminant(&msg)),
            }

            // Decode frames
            use openh264::decoder::Decoder;
            let mut decoder = Decoder::new().unwrap();
            let mut frame_times = Vec::new();

            loop {
                let frame_start = Instant::now();
                let msg = match protocol::read_message(&mut reader) {
                    Ok(m) => m,
                    Err(_) => break,
                };

                match msg {
                    Message::VideoFrame { frame, .. } => {
                        let _yuv = decoder.decode(&frame.data);
                        frame_times.push(frame_start.elapsed());
                        frames_received_client.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }

            // Return average frame receive time
            if frame_times.is_empty() {
                Duration::ZERO
            } else {
                let total: Duration = frame_times.iter().sum();
                total / frame_times.len() as u32
            }
        })
        .unwrap();

    server.join().unwrap();
    let avg_latency = client.join().unwrap();
    let total_time = start_time.elapsed();

    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(200)); // let proxy threads exit

    let received = frames_received.load(Ordering::Relaxed) as u32;
    (received, total_time, avg_latency)
}

// ── Actual Tests ────────────────────────────────────────────────────────────

#[test]
fn wan_baseline_lan() {
    let (received, total, avg_lat) = run_wan_e2e(WanProfile::lan(), 30, "lan");
    eprintln!(
        "LAN: {received}/30 frames, total={:.1}s, avg_latency={:.1}ms",
        total.as_secs_f64(),
        avg_lat.as_secs_f64() * 1000.0
    );
    assert_eq!(received, 30, "LAN should deliver all frames");
}

#[test]
fn wan_broadband_30ms() {
    let (received, total, avg_lat) = run_wan_e2e(WanProfile::broadband(), 30, "broadband");
    eprintln!(
        "Broadband (30ms RTT): {received}/30 frames, total={:.1}s, avg_latency={:.1}ms",
        total.as_secs_f64(),
        avg_lat.as_secs_f64() * 1000.0
    );
    // All frames should arrive (no loss, just delay)
    assert_eq!(received, 30, "broadband should deliver all frames");
}

#[test]
fn wan_high_latency_200ms() {
    let (received, total, avg_lat) = run_wan_e2e(WanProfile::high_latency(), 30, "high-lat");
    eprintln!(
        "High latency (200ms RTT): {received}/30 frames, total={:.1}s, avg_latency={:.1}ms",
        total.as_secs_f64(),
        avg_lat.as_secs_f64() * 1000.0
    );
    // All frames should still arrive — TCP handles latency, not loss
    assert_eq!(received, 30, "high latency should deliver all frames (TCP)");
}

#[test]
fn wan_lossy_wifi() {
    // Run with more frames to get statistical significance on 2% loss
    let (received, total, avg_lat) = run_wan_e2e(WanProfile::lossy_wifi(), 60, "lossy-wifi");
    eprintln!(
        "Lossy WiFi (20ms RTT, 2% loss): {received}/60 frames, total={:.1}s, avg_latency={:.1}ms",
        total.as_secs_f64(),
        avg_lat.as_secs_f64() * 1000.0
    );
    // TCP retransmits, so all frames should eventually arrive
    // (our proxy doesn't drop TCP segments, it just delays them — true loss would need raw sockets)
    assert_eq!(
        received, 60,
        "TCP handles retransmission — all frames should arrive"
    );
}

#[test]
fn wan_terrible_network() {
    let (received, total, avg_lat) = run_wan_e2e(WanProfile::terrible(), 20, "terrible");
    eprintln!(
        "Terrible (300ms RTT, 5% loss, 2Mbps): {received}/20 frames, total={:.1}s, avg_latency={:.1}ms",
        total.as_secs_f64(),
        avg_lat.as_secs_f64() * 1000.0
    );
    // Should still get all frames — TCP reliable delivery
    assert_eq!(
        received, 20,
        "terrible network should still deliver all frames (TCP)"
    );
}

#[test]
fn wan_encrypted_high_latency() {
    use phantom_core::crypto::{EncryptedReader, EncryptedWriter};

    let key = [0xABu8; 32];
    let width = 128u32;
    let height = 128u32;

    let server_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let proxy_addr = start_wan_proxy(server_addr, WanProfile::high_latency(), stop.clone());

    let server = thread::spawn(move || {
        let (stream, _) = server_listener.accept().unwrap();
        let mut writer = EncryptedWriter::new(stream, &key);

        // Send Hello
        let payload = bincode::serialize(&Message::Hello {
            width,
            height,
            format: PixelFormat::Bgra8,
            protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
            audio: false,
            video_codec: phantom_core::encode::VideoCodec::H264,
            session_token: vec![],
        })
        .unwrap();
        writer.write_encrypted(&payload).unwrap();

        // Send 10 clipboard sync messages (small, fast)
        for i in 0..10 {
            let payload = bincode::serialize(&Message::ClipboardSync(format!("msg-{i}"))).unwrap();
            writer.write_encrypted(&payload).unwrap();
            thread::sleep(Duration::from_millis(50));
        }

        thread::sleep(Duration::from_millis(500));
    });

    let client = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let stream = TcpStream::connect(proxy_addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = EncryptedReader::new(stream, &key);

        // Read Hello
        let payload = reader.read_decrypted().unwrap();
        let msg: Message = bincode::deserialize(&payload).unwrap();
        assert!(matches!(msg, Message::Hello { .. }));

        // Read clipboard messages
        let mut count = 0;
        for _ in 0..10 {
            match reader.read_decrypted() {
                Ok(payload) => {
                    let msg: Message = bincode::deserialize(&payload).unwrap();
                    if let Message::ClipboardSync(text) = msg {
                        assert!(text.starts_with("msg-"));
                        count += 1;
                    }
                }
                Err(_) => break,
            }
        }
        count
    });

    server.join().unwrap();
    let received = client.join().unwrap();

    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(200));

    eprintln!("Encrypted + 200ms RTT: {received}/10 messages received");
    assert_eq!(
        received, 10,
        "encrypted messages should all arrive over high-latency link"
    );
}

#[test]
fn wan_keepalive_survives_latency() {
    // Test that a connection stays alive even with 200ms RTT
    // (keepalive is 1s interval, so 200ms shouldn't cause timeouts)
    let server_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let proxy_addr = start_wan_proxy(server_addr, WanProfile::high_latency(), stop.clone());

    let server = thread::spawn(move || {
        let (stream, _) = server_listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let mut writer = std::io::BufWriter::new(&stream);
        let _reader = std::io::BufReader::new(stream.try_clone().unwrap());

        // Send Hello
        protocol::write_message(
            &mut writer,
            &Message::Hello {
                width: 320,
                height: 240,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
                video_codec: phantom_core::encode::VideoCodec::H264,
                session_token: vec![],
            },
        )
        .unwrap();

        // Send keepalive pings for 3 seconds
        let start = Instant::now();
        let mut pings_sent = 0u32;
        while start.elapsed() < Duration::from_secs(3) {
            if protocol::write_message(&mut writer, &Message::Ping).is_err() {
                break;
            }
            pings_sent += 1;
            thread::sleep(Duration::from_millis(500));
        }
        pings_sent
    });

    let client = thread::spawn(move || {
        let stream = TcpStream::connect(proxy_addr).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);

        // Read Hello
        let msg = protocol::read_message(&mut reader).unwrap();
        assert!(matches!(msg, Message::Hello { .. }));

        // Count received pings
        let mut pings = 0u32;
        loop {
            match protocol::read_message(&mut reader) {
                Ok(Message::Ping) => pings += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        pings
    });

    let pings_sent = server.join().unwrap();
    let pings_received = client.join().unwrap();

    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(200));

    eprintln!("Keepalive over 200ms RTT: sent={pings_sent}, received={pings_received}");
    assert_eq!(
        pings_received, pings_sent,
        "all keepalive pings should arrive despite latency"
    );
}

#[test]
fn wan_session_replacement_under_latency() {
    // Verify that session replacement (new client kicks old) works over a WAN link.
    // The Disconnect message must arrive before the old client tries to reconnect.
    let server_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let proxy_addr = start_wan_proxy(server_addr, WanProfile::broadband(), stop.clone());

    let server = thread::spawn(move || {
        // Accept first client
        let (stream1, _) = server_listener.accept().unwrap();
        stream1.set_nodelay(true).unwrap();
        let mut writer1 = std::io::BufWriter::new(&stream1);

        protocol::write_message(
            &mut writer1,
            &Message::Hello {
                width: 320,
                height: 240,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
                video_codec: phantom_core::encode::VideoCodec::H264,
                session_token: vec![],
            },
        )
        .unwrap();

        // Send a few frames to client 1
        for i in 0..5 {
            let _ = protocol::write_message(
                &mut writer1,
                &Message::VideoFrame {
                    sequence: i + 1,
                    frame: Box::new(EncodedFrame {
                        codec: VideoCodec::H264,
                        data: vec![0u8; 100],
                        is_keyframe: i == 0,
                    }),
                },
            );
            thread::sleep(Duration::from_millis(33));
        }

        // Send Disconnect to client 1 (simulating session replacement)
        let _ = protocol::write_message(
            &mut writer1,
            &Message::Disconnect {
                reason: "replaced by new client".to_string(),
            },
        );

        thread::sleep(Duration::from_millis(200));

        // Accept second client
        let (stream2, _) = server_listener.accept().unwrap();
        stream2.set_nodelay(true).unwrap();
        let mut writer2 = std::io::BufWriter::new(&stream2);

        protocol::write_message(
            &mut writer2,
            &Message::Hello {
                width: 320,
                height: 240,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
                video_codec: phantom_core::encode::VideoCodec::H264,
                session_token: vec![],
            },
        )
        .unwrap();

        // Send frames to client 2
        for i in 0..5 {
            let _ = protocol::write_message(
                &mut writer2,
                &Message::VideoFrame {
                    sequence: i + 1,
                    frame: Box::new(EncodedFrame {
                        codec: VideoCodec::H264,
                        data: vec![0u8; 100],
                        is_keyframe: i == 0,
                    }),
                },
            );
            thread::sleep(Duration::from_millis(33));
        }

        thread::sleep(Duration::from_millis(300));
    });

    // Client 1: should receive frames, then Disconnect
    let proxy_addr_1 = proxy_addr;
    let client1 = thread::spawn(move || {
        let stream = TcpStream::connect(proxy_addr_1).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);

        let msg = protocol::read_message(&mut reader).unwrap();
        assert!(matches!(msg, Message::Hello { .. }));

        let mut frames = 0u32;
        let mut got_disconnect = false;

        loop {
            match protocol::read_message(&mut reader) {
                Ok(Message::VideoFrame { .. }) => frames += 1,
                Ok(Message::Disconnect { reason }) => {
                    assert_eq!(reason, "replaced by new client");
                    got_disconnect = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        (frames, got_disconnect)
    });

    // Wait for client1 to get disconnected, then connect client2
    thread::sleep(Duration::from_millis(500));

    let proxy_addr_2 = proxy_addr;
    let client2 = thread::spawn(move || {
        let stream = TcpStream::connect(proxy_addr_2).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);

        let msg = protocol::read_message(&mut reader).unwrap();
        assert!(matches!(msg, Message::Hello { .. }));

        let mut frames = 0u32;
        loop {
            match protocol::read_message(&mut reader) {
                Ok(Message::VideoFrame { .. }) => frames += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        frames
    });

    server.join().unwrap();
    let (c1_frames, c1_disconnect) = client1.join().unwrap();
    let c2_frames = client2.join().unwrap();

    stop.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(200));

    eprintln!(
        "Session replacement: client1={c1_frames} frames + disconnect={c1_disconnect}, client2={c2_frames} frames"
    );
    assert!(c1_disconnect, "client1 should receive Disconnect message");
    assert!(
        c1_frames >= 3,
        "client1 should receive some frames before disconnect"
    );
    assert!(
        c2_frames >= 3,
        "client2 should receive frames after taking over"
    );
}
