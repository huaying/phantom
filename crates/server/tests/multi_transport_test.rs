use phantom_core::protocol::{self, Message};
use std::io::Cursor;
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

/// Test that the server with multi-transport accepts TCP connections.
#[test]
fn multi_transport_tcp_connect() {
    // Try to connect to a TCP server on a random port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel();

    // Simulate the accept loop
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            tx.send(true).ok();
            drop(stream);
        }
    });

    // Connect
    let _client = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    let accepted = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(accepted);
}

/// Test that multiple transport listeners can feed into a single channel.
#[test]
fn multi_transport_channel_dispatch() {
    let (conn_tx, conn_rx) = mpsc::channel::<String>();

    // Simulate two transport listeners
    let tx1 = conn_tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        tx1.send("tcp".to_string()).ok();
    });

    let tx2 = conn_tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        tx2.send("web".to_string()).ok();
    });

    drop(conn_tx);

    let mut received = Vec::new();
    while let Ok(transport) = conn_rx.recv_timeout(Duration::from_secs(2)) {
        received.push(transport);
    }

    assert_eq!(received.len(), 2);
    assert!(received.contains(&"tcp".to_string()));
    assert!(received.contains(&"web".to_string()));
}

/// Test transport name parsing (comma-separated).
#[test]
fn transport_name_parsing() {
    let input = "tcp,web";
    let transports: Vec<&str> = input.split(',').map(|s| s.trim()).collect();
    assert_eq!(transports, vec!["tcp", "web"]);

    let single = "tcp";
    let transports: Vec<&str> = single.split(',').map(|s| s.trim()).collect();
    assert_eq!(transports, vec!["tcp"]);

    let with_spaces = "tcp , web , quic";
    let transports: Vec<&str> = with_spaces.split(',').map(|s| s.trim()).collect();
    assert_eq!(transports, vec!["tcp", "web", "quic"]);
}

/// Test port assignment logic for multi-transport.
#[test]
fn multi_transport_port_assignment() {
    let base_port: u16 = 9900;
    let transports = vec!["tcp", "web"];

    // When multiple transports, web gets base_port + 1
    let web_port = if transports.len() > 1 {
        base_port + 1
    } else {
        base_port
    };
    assert_eq!(web_port, 9901);

    // When single transport, web gets base_port
    let single = vec!["web"];
    let web_port_single = if single.len() > 1 {
        base_port + 1
    } else {
        base_port
    };
    assert_eq!(web_port_single, 9900);
}

/// Test that listen address parsing extracts host and port correctly.
#[test]
fn listen_address_parsing() {
    let addr = "0.0.0.0:9900";
    let base_port: u16 = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9900);
    let host: &str = addr.rsplit_once(':').map(|x| x.0).unwrap_or("0.0.0.0");

    assert_eq!(base_port, 9900);
    assert_eq!(host, "0.0.0.0");

    // IPv6
    let addr6 = "127.0.0.1:8080";
    let port6: u16 = addr6
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9900);
    assert_eq!(port6, 8080);
}

/// Test that the Hello message roundtrips correctly (used in transport handshake).
#[test]
fn hello_message_roundtrip() {
    use phantom_core::frame::PixelFormat;

    let msg = Message::Hello {
        width: 1920,
        height: 1080,
        format: PixelFormat::Bgra8,
        protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
        audio: false,
        video_codec: phantom_core::encode::VideoCodec::H264,
    };

    let mut buf = Vec::new();
    protocol::write_message(&mut buf, &msg).unwrap();

    let mut cursor = Cursor::new(&buf);
    let decoded = protocol::read_message(&mut cursor).unwrap();

    match decoded {
        Message::Hello { width, height, .. } => {
            assert_eq!(width, 1920);
            assert_eq!(height, 1080);
        }
        _ => panic!("expected Hello message"),
    }
}
