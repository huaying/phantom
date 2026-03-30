use anyhow::{bail, Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::net::Protocol;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

/// Negotiate WebRTC DataChannel over existing WS signaling.
pub fn negotiate(
    signaling_tx: &mpsc::Sender<String>,
    signaling_rx: &mpsc::Receiver<String>,
    udp_port: u16,
) -> Result<(WebRtcSender, WebRtcReceiver)> {
    // Bind UDP socket
    let socket = UdpSocket::bind(format!("0.0.0.0:{udp_port}"))
        .context("bind UDP")?;
    let local_addr = socket.local_addr()?;
    tracing::info!(%local_addr, "WebRTC UDP bound");

    // Wait for SDP offer from browser (skip any ICE candidates that arrive first)
    let mut sdp_str = String::new();
    let deadline_offer = Instant::now() + Duration::from_secs(10);
    let mut early_candidates = Vec::new();
    loop {
        let msg = signaling_rx
            .recv_timeout(deadline_offer.saturating_duration_since(Instant::now()).max(Duration::from_millis(100)))
            .context("timeout waiting for offer")?;
        let val: serde_json::Value = serde_json::from_str(&msg)?;
        match val["type"].as_str() {
            Some("offer") => {
                sdp_str = val["sdp"].as_str().context("missing sdp")?.to_string();
                break;
            }
            Some("candidate") => {
                // ICE candidates may arrive before offer — buffer them
                early_candidates.push(msg);
            }
            other => {
                tracing::debug!("ignoring signaling message: {:?}", other);
            }
        }
    }

    // Create Rtc + accept offer
    let mut rtc = Rtc::builder().build();

    // Add host candidate with actual IP (not 0.0.0.0)
    // Use the Docker container's network interface IP
    let host_ip = get_local_ip().unwrap_or([127, 0, 0, 1].into());
    let candidate_addr = std::net::SocketAddr::new(host_ip, local_addr.port());
    let candidate = Candidate::host(candidate_addr, "udp").context("host candidate")?;
    rtc.add_local_candidate(candidate);

    let offer = str0m::change::SdpOffer::from_sdp_string(&sdp_str)
        .context("parse SDP")?;
    let answer = rtc.sdp_api().accept_offer(offer)
        .context("accept offer")?;

    // Send answer
    signaling_tx.send(serde_json::json!({
        "type": "answer",
        "sdp": answer.to_sdp_string(),
    }).to_string()).map_err(|_| anyhow::anyhow!("signaling closed"))?;

    // Run connection loop — poll until DataChannels open
    socket.set_read_timeout(Some(Duration::from_millis(5)))?;
    let mut buf = [0u8; 2000];

    let mut video_ch: Option<ChannelId> = None;
    let mut input_ch: Option<ChannelId> = None;
    let mut control_ch: Option<ChannelId> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        // Poll str0m outputs
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(Event::ChannelOpen(id, label))) => {
                    tracing::info!(%label, "DataChannel opened");
                    match label.as_str() {
                        "video" => video_ch = Some(id),
                        "input" => input_ch = Some(id),
                        "control" => control_ch = Some(id),
                        _ => {}
                    }
                }
                Ok(Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected))) => {
                    bail!("ICE disconnected");
                }
                Ok(Output::Timeout(_)) => break, // No more output, proceed to recv
                Ok(_) => {}
                Err(e) => { tracing::debug!("poll error: {e}"); break; }
            }
        }

        // Read UDP
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let r = str0m::net::Receive {
                    proto: Protocol::Udp,
                    source: addr,
                    destination: local_addr,
                    contents: match (&buf[..n]).try_into() {
                        Ok(c) => c,
                        Err(_) => continue,
                    },
                };
                let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }

        rtc.handle_input(Input::Timeout(Instant::now())).ok();

        // Check if all 3 channels are open
        if video_ch.is_some() && input_ch.is_some() && control_ch.is_some() {
            break;
        }
    }

    let video_id = video_ch.context("video DC not opened")?;
    let input_id = input_ch.context("input DC not opened")?;
    let control_id = control_ch.context("control DC not opened")?;

    tracing::info!("all DataChannels open, switching to WebRTC");
    signaling_tx.send(r#"{"type":"ready"}"#.to_string()).ok();

    // Spawn IO loop
    let (video_tx, video_rx) = mpsc::channel::<Vec<u8>>();
    let (control_out_tx, control_out_rx) = mpsc::channel::<Vec<u8>>();
    let (input_in_tx, input_in_rx) = mpsc::channel::<Vec<u8>>();
    let (control_in_tx, control_in_rx) = mpsc::channel::<Vec<u8>>();

    std::thread::spawn(move || {
        rtc_io_loop(
            rtc, socket, video_id, input_id, control_id,
            video_rx, control_out_rx, input_in_tx, control_in_tx,
        );
    });

    Ok((
        WebRtcSender { video_tx, control_tx: control_out_tx },
        WebRtcReceiver { input_rx: input_in_rx, control_rx: control_in_rx },
    ))
}

fn rtc_io_loop(
    mut rtc: Rtc,
    socket: UdpSocket,
    video_id: ChannelId,
    input_id: ChannelId,
    control_id: ChannelId,
    video_rx: mpsc::Receiver<Vec<u8>>,
    control_out_rx: mpsc::Receiver<Vec<u8>>,
    input_in_tx: mpsc::Sender<Vec<u8>>,
    control_in_tx: mpsc::Sender<Vec<u8>>,
) {
    let local_addr = socket.local_addr().unwrap();
    let _ = socket.set_read_timeout(Some(Duration::from_millis(5)));
    let mut buf = [0u8; 65535];

    loop {
        // Send outgoing data through DataChannels
        while let Ok(data) = video_rx.try_recv() {
            if let Some(mut ch) = rtc.channel(video_id) {
                let _ = ch.write(true, &data);
            }
        }
        while let Ok(data) = control_out_rx.try_recv() {
            if let Some(mut ch) = rtc.channel(control_id) {
                let _ = ch.write(true, &data);
            }
        }

        // Poll str0m
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(Event::ChannelData(cd))) => {
                    if cd.id == input_id {
                        if input_in_tx.send(cd.data).is_err() { return; }
                    } else if cd.id == control_id {
                        if control_in_tx.send(cd.data).is_err() { return; }
                    }
                }
                Ok(Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected))) => {
                    tracing::info!("WebRTC disconnected");
                    return;
                }
                Ok(Output::Timeout(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // Receive UDP
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let r = str0m::net::Receive {
                    proto: Protocol::Udp,
                    source: addr,
                    destination: local_addr,
                    contents: match (&buf[..n]).try_into() {
                        Ok(c) => c,
                        Err(_) => continue,
                    },
                };
                let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }

        rtc.handle_input(Input::Timeout(Instant::now())).ok();
    }
}

fn get_local_ip() -> Option<std::net::IpAddr> {
    // Connect to a public IP to determine our local interface address
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

pub struct WebRtcSender {
    video_tx: mpsc::Sender<Vec<u8>>,
    control_tx: mpsc::Sender<Vec<u8>>,
}

impl MessageSender for WebRtcSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
        match msg {
            Message::VideoFrame { .. } | Message::TileUpdate { .. } => {
                self.video_tx.send(payload).map_err(|_| anyhow::anyhow!("video DC closed"))
            }
            _ => {
                self.control_tx.send(payload).map_err(|_| anyhow::anyhow!("control DC closed"))
            }
        }
    }
}

pub struct WebRtcReceiver {
    input_rx: mpsc::Receiver<Vec<u8>>,
    control_rx: mpsc::Receiver<Vec<u8>>,
}

impl MessageReceiver for WebRtcReceiver {
    fn recv_msg(&mut self) -> Result<Message> {
        loop {
            if let Ok(data) = self.input_rx.try_recv() {
                return bincode::deserialize(&data).context("deserialize");
            }
            if let Ok(data) = self.control_rx.try_recv() {
                return bincode::deserialize(&data).context("deserialize");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
