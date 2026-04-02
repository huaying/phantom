use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::net::UdpSocket;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::net::Protocol;
use str0m::{Event, Input, Output, Rtc};

/// Max single DataChannel message size. str0m's SCTP silently fails on
/// messages larger than ~16KB. Split into chunks with reassembly on client.
const DC_CHUNK_SIZE: usize = 16_384;

/// A single WebRTC run loop managing one client at a time.
/// Uses the str0m official pattern: one UDP socket, one loop, demux via accepts().
///
/// Lifecycle:
///   1. Loop waits for Rtc from POST /rtc (via channel)
///   2. Drives ICE/DTLS/SCTP until DataChannels open
///   3. Bridges data between DataChannels and MessageSender/Receiver channels
///   4. When client disconnects, cleans up and goes back to step 1
pub fn run_loop(
    candidate_addr: std::net::SocketAddr,
    rtc_rx: mpsc::Receiver<Rtc>,
    session_slot: Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
    notify_tx: mpsc::Sender<()>,
) {
    // One UDP socket for the entire server lifetime
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", candidate_addr.port())) {
        Ok(s) => s,
        Err(e) => { tracing::error!("bind UDP: {e}"); return; }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(1)));
    let mut buf = vec![0u8; 65535];

    tracing::info!(port = candidate_addr.port(), "WebRTC run loop started");

    // Current active client (None = waiting for connection)
    let mut active: Option<ActiveClient> = None;

    loop {
        // 1. Accept new Rtc from POST /rtc.
        //    Drain ALL pending — only keep the latest (browser may have refreshed multiple times).
        {
            let mut latest: Option<Rtc> = None;
            while let Ok(rtc) = rtc_rx.try_recv() {
                latest = Some(rtc);
            }
            if let Some(rtc) = latest {
                if active.is_some() {
                    tracing::info!("replacing old client (browser refreshed)");
                }
                tracing::info!("new WebRTC client from POST /rtc");
                active = Some(ActiveClient::new(rtc));
            }
        }

        // 2. Clean up disconnected client
        if let Some(ref client) = active {
            if !client.rtc.is_alive() || client.ice_disconnected {
                tracing::info!("WebRTC client disconnected (alive={}, ice_disconnected={})",
                    client.rtc.is_alive(), client.ice_disconnected);
                active = None;
            }
        }

        // 3. Poll active client's str0m outputs
        if let Some(ref mut client) = active {
            loop {
                match client.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(event)) => {
                        client.handle_event(event, &session_slot, &notify_tx);
                    }
                    Ok(Output::Timeout(_)) => break,
                    Err(_) => break,
                }
            }

            // Send outgoing data from session loop → DataChannels
            client.drain_outgoing();

            // Flush: poll_output again to immediately transmit data just written
            // by drain_outgoing. Without this, large messages (e.g. 70KB keyframe)
            // would wait until the next loop iteration to start transmitting.
            loop {
                match client.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(event)) => {
                        client.handle_event(event, &session_slot, &notify_tx);
                    }
                    Ok(Output::Timeout(_)) => break,
                    Err(_) => break,
                }
            }
        }

        // 4. Read UDP socket
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let Ok(contents) = (&buf[..n]).try_into() else { continue };
                let input = Input::Receive(
                    Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source: addr,
                        destination: candidate_addr,
                        contents,
                    },
                );

                if let Some(ref mut client) = active {
                    let _ = client.rtc.handle_input(input);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {}
        }

        // 5. Drive time forward
        if let Some(ref mut client) = active {
            client.rtc.handle_input(Input::Timeout(Instant::now())).ok();
        }
    }
}

struct ActiveClient {
    rtc: Rtc,
    video_id: Option<ChannelId>,
    input_id: Option<ChannelId>,
    control_id: Option<ChannelId>,
    channels_ready: bool,
    ice_disconnected: bool,
    /// Session loop sends data here → we write to DataChannels
    video_rx: Option<mpsc::Receiver<Vec<u8>>>,
    control_out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// We receive data from DataChannels → send to session loop
    input_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    control_in_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl ActiveClient {
    fn new(rtc: Rtc) -> Self {
        Self {
            rtc,
            video_id: None, input_id: None, control_id: None,
            channels_ready: false,
            ice_disconnected: false,
            video_rx: None, control_out_rx: None,
            input_in_tx: None, control_in_tx: None,
        }
    }

    fn handle_event(
        &mut self,
        event: Event,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        match event {
            Event::ChannelOpen(id, label) => {
                tracing::info!(%label, "DataChannel opened");
                match label.as_str() {
                    "video" => self.video_id = Some(id),
                    "input" => self.input_id = Some(id),
                    "control" => self.control_id = Some(id),
                    _ => {}
                }

                if self.video_id.is_some() && self.input_id.is_some()
                    && self.control_id.is_some() && !self.channels_ready
                {
                    tracing::info!("all 3 DataChannels open — session ready");
                    self.channels_ready = true;

                    // Bounded for session→runloop (video is high bandwidth)
                    let (video_tx, video_rx) = mpsc::sync_channel(30); // ~1s of frames
                    let (ctrl_out_tx, ctrl_out_rx) = mpsc::sync_channel(64);
                    // Unbounded for runloop→session (input/control are small + infrequent)
                    let (input_in_tx, input_in_rx) = mpsc::channel();
                    let (ctrl_in_tx, ctrl_in_rx) = mpsc::channel();

                    self.video_rx = Some(video_rx);
                    self.control_out_rx = Some(ctrl_out_rx);
                    self.input_in_tx = Some(input_in_tx);
                    self.control_in_tx = Some(ctrl_in_tx);

                    // Overwrite any stale session — main thread always gets the latest
                    *session_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((
                        WebRtcSender { video_tx, control_tx: ctrl_out_tx },
                        WebRtcReceiver { input_rx: input_in_rx, control_rx: ctrl_in_rx },
                    ));
                    let _ = notify_tx.send(());
                }
            }
            Event::ChannelData(cd) => {
                if Some(cd.id) == self.input_id {
                    if let Some(ref tx) = self.input_in_tx {
                        let _ = tx.send(cd.data);
                    }
                } else if Some(cd.id) == self.control_id {
                    if let Some(ref tx) = self.control_in_tx {
                        let _ = tx.send(cd.data);
                    }
                }
            }
            Event::IceConnectionStateChange(s) => {
                tracing::info!(?s, "ICE state");
                if matches!(s, str0m::IceConnectionState::Disconnected) {
                    self.ice_disconnected = true;
                }
            }
            _ => {}
        }
    }

    fn drain_outgoing(&mut self) {
        if !self.channels_ready { return; }

        // Collect messages to avoid borrow conflict with write_chunked
        let video_msgs: Vec<Vec<u8>> = self.video_rx.as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        let ctrl_msgs: Vec<Vec<u8>> = self.control_out_rx.as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        if let Some(vid) = self.video_id {
            for data in &video_msgs {
                self.write_chunked(vid, data);
            }
        }
        if let Some(ctrl) = self.control_id {
            for data in &ctrl_msgs {
                self.write_chunked(ctrl, data);
            }
        }
    }

    /// Write data to a DataChannel, splitting into chunks if larger than DC_CHUNK_SIZE.
    /// Small messages sent as-is. Large messages: each chunk = [u32 total_len LE][payload].
    fn write_chunked(&mut self, channel_id: ChannelId, data: &[u8]) {
        if data.len() <= DC_CHUNK_SIZE {
            if let Some(mut ch) = self.rtc.channel(channel_id) {
                let _ = ch.write(true, data);
            }
            return;
        }
        let total = data.len() as u32;
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + DC_CHUNK_SIZE - 4).min(data.len());
            let mut chunk = Vec::with_capacity(4 + (end - offset));
            chunk.extend_from_slice(&total.to_le_bytes());
            chunk.extend_from_slice(&data[offset..end]);
            if let Some(mut ch) = self.rtc.channel(channel_id) {
                let _ = ch.write(true, &chunk);
            }
            offset = end;
        }
    }
}

pub struct WebRtcSender {
    video_tx: mpsc::SyncSender<Vec<u8>>,
    control_tx: mpsc::SyncSender<Vec<u8>>,
}

impl MessageSender for WebRtcSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        let payload = bincode::serialize(msg).context("serialize")?;
        match msg {
            // Video data (including Hello) → video DC (reliable + ordered)
            Message::Hello { .. } | Message::VideoFrame { .. } | Message::TileUpdate { .. } => {
                self.video_tx.try_send(payload)
                    .map_err(|e| match e {
                        mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("video DC closed"),
                        mpsc::TrySendError::Full(_) => { /* backpressure */ anyhow::anyhow!("") },
                    })
                    .or(Ok(()))
            }
            _ => self.control_tx.try_send(payload)
                    .map_err(|e| match e {
                        mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("control DC closed"),
                        mpsc::TrySendError::Full(_) => anyhow::anyhow!(""),
                    })
                    .or(Ok(())),
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
            match self.input_rx.try_recv() {
                Ok(d) => return bincode::deserialize(&d).context("deserialize"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("input channel closed");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match self.control_rx.try_recv() {
                Ok(d) => return bincode::deserialize(&d).context("deserialize"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("control channel closed");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
