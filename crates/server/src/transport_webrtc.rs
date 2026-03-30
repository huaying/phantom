use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::net::Protocol;
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

/// Run an already-negotiated Rtc instance. Wait for DataChannels to open,
/// then start the IO loop. Returns (sender, receiver) for the session.
pub fn run_rtc(
    mut rtc: Rtc,
    udp_socket: &UdpSocket,
    local_addr: std::net::SocketAddr,
) -> Result<(WebRtcSender, WebRtcReceiver)> {
    let socket = udp_socket.try_clone().context("clone UDP")?;
    socket.set_read_timeout(Some(Duration::from_millis(5)))?;

    // Kick str0m's state machine to start ICE
    rtc.handle_input(Input::Timeout(Instant::now())).ok();

    // Wait for DataChannels to open (ICE + DTLS + SCTP handshake)
    let mut video_ch: Option<ChannelId> = None;
    let mut input_ch: Option<ChannelId> = None;
    let mut control_ch: Option<ChannelId> = None;
    let mut buf = [0u8; 2000];
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        // Poll str0m
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
                Ok(Output::Event(Event::IceConnectionStateChange(s))) => {
                    tracing::info!(?s, "ICE state");
                    if s == IceConnectionState::Disconnected {
                        anyhow::bail!("ICE disconnected");
                    }
                }
                Ok(Output::Timeout(_)) => break,
                Ok(_) => {}
                Err(e) => { tracing::debug!("poll: {e}"); break; }
            }
        }

        // Receive UDP
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if let Ok(contents) = (&buf[..n]).try_into() {
                    let r = str0m::net::Receive {
                        proto: Protocol::Udp,
                        source: addr,
                        destination: local_addr,
                        contents,
                    };
                    let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
        rtc.handle_input(Input::Timeout(Instant::now())).ok();

        // All 3 channels open?
        if video_ch.is_some() && input_ch.is_some() && control_ch.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let video_id = video_ch.context("video DC not opened")?;
    let input_id = input_ch.context("input DC not opened")?;
    let control_id = control_ch.context("control DC not opened")?;
    tracing::info!("all 3 DataChannels open");

    // Spawn IO loop
    let (video_tx, video_rx) = mpsc::channel::<Vec<u8>>();
    let (ctrl_out_tx, ctrl_out_rx) = mpsc::channel::<Vec<u8>>();
    let (input_in_tx, input_in_rx) = mpsc::channel::<Vec<u8>>();
    let (ctrl_in_tx, ctrl_in_rx) = mpsc::channel::<Vec<u8>>();

    std::thread::spawn(move || {
        io_loop(rtc, socket, local_addr, video_id, input_id, control_id,
            video_rx, ctrl_out_rx, input_in_tx, ctrl_in_tx);
    });

    Ok((
        WebRtcSender { video_tx, control_tx: ctrl_out_tx },
        WebRtcReceiver { input_rx: input_in_rx, control_rx: ctrl_in_rx },
    ))
}

fn io_loop(
    mut rtc: Rtc, socket: UdpSocket, local_addr: std::net::SocketAddr,
    video_id: ChannelId, input_id: ChannelId, control_id: ChannelId,
    video_rx: mpsc::Receiver<Vec<u8>>, ctrl_out_rx: mpsc::Receiver<Vec<u8>>,
    input_in_tx: mpsc::Sender<Vec<u8>>, ctrl_in_tx: mpsc::Sender<Vec<u8>>,
) {
    let mut buf = [0u8; 65535];
    loop {
        // Send outgoing
        while let Ok(data) = video_rx.try_recv() {
            if let Some(mut ch) = rtc.channel(video_id) { let _ = ch.write(true, &data); }
        }
        while let Ok(data) = ctrl_out_rx.try_recv() {
            if let Some(mut ch) = rtc.channel(control_id) { let _ = ch.write(true, &data); }
        }

        // Poll str0m
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => { let _ = socket.send_to(&t.contents, t.destination); }
                Ok(Output::Event(Event::ChannelData(cd))) => {
                    if cd.id == input_id {
                        if input_in_tx.send(cd.data).is_err() { return; }
                    } else if cd.id == control_id {
                        if ctrl_in_tx.send(cd.data).is_err() { return; }
                    }
                }
                Ok(Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected))) => return,
                Ok(Output::Timeout(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // Receive UDP
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if let Ok(contents) = (&buf[..n]).try_into() {
                    let r = str0m::net::Receive {
                        proto: Protocol::Udp, source: addr, destination: local_addr, contents,
                    };
                    let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
        rtc.handle_input(Input::Timeout(Instant::now())).ok();
    }
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
            _ => self.control_tx.send(payload).map_err(|_| anyhow::anyhow!("control DC closed")),
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
            if let Ok(d) = self.input_rx.try_recv() { return bincode::deserialize(&d).context("de"); }
            if let Ok(d) = self.control_rx.try_recv() { return bincode::deserialize(&d).context("de"); }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
