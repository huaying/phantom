use anyhow::{bail, Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::net::Protocol;
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

/// Negotiate WebRTC over the existing WebSocket signaling channel.
/// Returns (sender, receiver) if successful, or error to fallback to WS.
pub fn negotiate(
    signaling_tx: &mpsc::Sender<String>,
    signaling_rx: &mpsc::Receiver<String>,
    udp_port: u16,
) -> Result<(WebRtcSender, WebRtcReceiver)> {
    let socket = UdpSocket::bind(format!("0.0.0.0:{udp_port}"))
        .context("bind UDP for WebRTC")?;
    socket.set_read_timeout(Some(Duration::from_millis(5)))?;
    let local_addr = socket.local_addr()?;
    tracing::info!(%local_addr, "WebRTC UDP bound");

    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .build();

    // Wait for SDP offer from browser
    let offer_json = signaling_rx
        .recv_timeout(Duration::from_secs(10))
        .context("timeout waiting for WebRTC offer")?;

    let offer_val: serde_json::Value = serde_json::from_str(&offer_json)?;
    if offer_val["type"].as_str() != Some("offer") {
        bail!("expected offer, got: {}", offer_val["type"]);
    }
    let sdp_str = offer_val["sdp"].as_str().context("missing sdp")?;

    let offer = str0m::change::SdpOffer::from_sdp_string(sdp_str)
        .context("parse SDP offer")?;
    let answer = rtc.sdp_api().accept_offer(offer)
        .context("accept offer")?;

    // Send answer
    let answer_json = serde_json::json!({
        "type": "answer",
        "sdp": answer.to_sdp_string(),
    });
    signaling_tx.send(answer_json.to_string())
        .map_err(|_| anyhow::anyhow!("signaling closed"))?;

    // Run ICE/DTLS until DataChannels open
    let mut video_ch: Option<ChannelId> = None;
    let mut input_ch: Option<ChannelId> = None;
    let mut control_ch: Option<ChannelId> = None;
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        // Process signaling (ICE candidates)
        while let Ok(msg) = signaling_rx.try_recv() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&msg) {
                if val["type"].as_str() == Some("candidate") {
                    tracing::debug!("received ICE candidate");
                }
            }
        }

        // Poll str0m
        match rtc.poll_output() {
            Ok(Output::Transmit(t)) => {
                let _ = socket.send_to(&t.contents, t.destination);
            }
            Ok(Output::Event(Event::ChannelOpen(_, label))) => {
                tracing::info!(%label, "DataChannel opened");
                // We need to look up channel by label after it opens
                // str0m assigns ChannelIds internally
            }
            Ok(Output::Event(Event::ChannelData(_))) => {
                // Data during setup, ignore
            }
            Ok(Output::Event(Event::IceConnectionStateChange(state))) => {
                tracing::info!(?state, "ICE state");
                if state == IceConnectionState::Disconnected {
                    bail!("ICE disconnected during setup");
                }
            }
            Ok(Output::Timeout(_)) => {}
            Ok(Output::Event(_)) => {}
            Err(e) => {
                tracing::debug!("str0m poll error: {e}");
            }
        }

        // Receive UDP
        let mut buf = [0u8; 65535];
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let input = Input::Receive(
                    Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source: addr,
                        destination: local_addr,
                        contents: match (&buf[..n]).try_into() {
                            Ok(c) => c,
                            Err(_) => continue,
                        },
                    },
                );
                let _ = rtc.handle_input(input);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }

        let _ = rtc.handle_input(Input::Timeout(Instant::now()));

        // Check channels by iterating (str0m may have opened them)
        // str0m's channel API: we detect open channels via events
        // For now, use a simpler approach: check after ICE connects
        // The ChannelOpen events give us (ChannelId, label)
        // TODO: properly track channel IDs from ChannelOpen events

        std::thread::sleep(Duration::from_millis(1));
    }

    // For now, bail — the full implementation needs proper channel tracking
    // which requires more str0m API exploration
    bail!("WebRTC negotiation not yet fully implemented — falling back to WebSocket")
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
