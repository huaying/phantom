use anyhow::{Context, Result};
use phantom_core::protocol::Message;
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::net::Protocol;
use str0m::{Event, Input, Output, Rtc};

/// Max single DataChannel chunk size for the wire.
/// SCTP fragments internally, but str0m's `available()` check against the
/// 128 KB cross-stream buffer means large writes get rejected when the buffer
/// is partially full. We use small-ish chunks so each individual write is
/// likely to fit, and queue the rest for backpressure-based draining.
const DC_CHUNK_SIZE: usize = 16_384;

/// Threshold for `set_buffered_amount_low_threshold`. When the channel's
/// buffered amount drops below this, str0m fires `ChannelBufferedAmountLow`
/// and we resume draining the pending queue.
const BUFFERED_LOW_THRESHOLD: usize = 32_768; // 32 KB

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
            client.poll_and_flush(&socket, &session_slot, &notify_tx);

            // Send outgoing data from session loop → DataChannels
            client.drain_outgoing();

            // Flush: transmit data just written by drain_outgoing.
            client.poll_and_flush(&socket, &session_slot, &notify_tx);
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

/// Per-channel pending write queue for backpressure.
struct PendingQueue {
    /// Chunks waiting to be written (already framed with [total_len][payload]).
    queue: VecDeque<Vec<u8>>,
    /// Whether we're in backpressure mode (waiting for BufferedAmountLow).
    paused: bool,
}

impl PendingQueue {
    fn new() -> Self {
        Self { queue: VecDeque::new(), paused: false }
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
    /// Per-channel backpressure queues
    video_pending: PendingQueue,
    control_pending: PendingQueue,
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
            video_pending: PendingQueue::new(),
            control_pending: PendingQueue::new(),
        }
    }

    /// Poll str0m for outputs, transmit UDP, and handle events (including
    /// BufferedAmountLow which resumes backpressured channels).
    fn poll_and_flush(
        &mut self,
        socket: &UdpSocket,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => {
                    self.handle_event(event, session_slot, notify_tx);
                }
                Ok(Output::Timeout(_)) => break,
                Err(_) => break,
            }
        }

        // After flushing, try to drain pending queues (BufferedAmountLow may have unpaused them)
        if let Some(vid) = self.video_id {
            self.flush_pending(vid, &mut PendingRef::Video);
        }
        if let Some(ctrl) = self.control_id {
            self.flush_pending(ctrl, &mut PendingRef::Control);
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
                    "video" => {
                        self.video_id = Some(id);
                        // Set backpressure threshold
                        if let Some(mut ch) = self.rtc.channel(id) {
                            ch.set_buffered_amount_low_threshold(BUFFERED_LOW_THRESHOLD);
                        }
                    }
                    "input" => self.input_id = Some(id),
                    "control" => {
                        self.control_id = Some(id);
                        if let Some(mut ch) = self.rtc.channel(id) {
                            ch.set_buffered_amount_low_threshold(BUFFERED_LOW_THRESHOLD);
                        }
                    }
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
            Event::ChannelBufferedAmountLow(id) => {
                // Buffer drained — resume sending pending chunks
                if Some(id) == self.video_id {
                    self.video_pending.paused = false;
                    tracing::trace!("video DC buffer low — resuming");
                } else if Some(id) == self.control_id {
                    self.control_pending.paused = false;
                    tracing::trace!("control DC buffer low — resuming");
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

    /// Drain outgoing session data into DataChannels, respecting backpressure.
    fn drain_outgoing(&mut self) {
        if !self.channels_ready { return; }

        // First: try to flush any pending chunks from previous iterations
        if let Some(vid) = self.video_id {
            self.flush_pending(vid, &mut PendingRef::Video);
        }
        if let Some(ctrl) = self.control_id {
            self.flush_pending(ctrl, &mut PendingRef::Control);
        }

        // Then: pull new messages from session loop
        let video_msgs: Vec<Vec<u8>> = self.video_rx.as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        let ctrl_msgs: Vec<Vec<u8>> = self.control_out_rx.as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        if let Some(vid) = self.video_id {
            for data in &video_msgs {
                self.write_with_backpressure(vid, data, &mut PendingRef::Video);
            }
        }
        if let Some(ctrl) = self.control_id {
            for data in &ctrl_msgs {
                self.write_with_backpressure(ctrl, data, &mut PendingRef::Control);
            }
        }
    }

    /// Write data to a DataChannel with proper backpressure handling.
    /// Splits large messages into chunks. If a write is rejected (buffer full),
    /// remaining chunks are queued and we wait for BufferedAmountLow.
    fn write_with_backpressure(
        &mut self, channel_id: ChannelId, data: &[u8], pending: &mut PendingRef,
    ) {
        let pending_q = match pending {
            PendingRef::Video => &mut self.video_pending,
            PendingRef::Control => &mut self.control_pending,
        };

        // If already paused, queue everything
        if pending_q.paused {
            self.enqueue_chunked(data, pending);
            return;
        }

        if data.len() <= DC_CHUNK_SIZE {
            // Small message — try direct write
            if let Some(mut ch) = self.rtc.channel(channel_id) {
                match ch.write(true, data) {
                    Ok(true) => {} // success
                    Ok(false) => {
                        // Buffer full — queue and pause
                        tracing::debug!(
                            channel = ?channel_id,
                            size = data.len(),
                            "DC write rejected (buffer full) — enabling backpressure"
                        );
                        let pending_q = match pending {
                            PendingRef::Video => &mut self.video_pending,
                            PendingRef::Control => &mut self.control_pending,
                        };
                        pending_q.queue.push_back(data.to_vec());
                        pending_q.paused = true;
                    }
                    Err(e) => {
                        tracing::warn!("DC write error: {e}");
                    }
                }
            }
            return;
        }

        // Large message — chunk it
        let total = data.len() as u32;
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + DC_CHUNK_SIZE - 4).min(data.len());
            let mut chunk = Vec::with_capacity(4 + (end - offset));
            chunk.extend_from_slice(&total.to_le_bytes());
            chunk.extend_from_slice(&data[offset..end]);

            let pending_q = match pending {
                PendingRef::Video => &mut self.video_pending,
                PendingRef::Control => &mut self.control_pending,
            };

            if pending_q.paused {
                // Already in backpressure — queue remaining chunks
                pending_q.queue.push_back(chunk);
            } else if let Some(mut ch) = self.rtc.channel(channel_id) {
                match ch.write(true, &chunk) {
                    Ok(true) => {} // success
                    Ok(false) => {
                        // Buffer full — queue this and remaining chunks
                        tracing::debug!(
                            channel = ?channel_id,
                            offset,
                            total = data.len(),
                            "DC chunk write rejected — enabling backpressure"
                        );
                        let pending_q = match pending {
                            PendingRef::Video => &mut self.video_pending,
                            PendingRef::Control => &mut self.control_pending,
                        };
                        pending_q.queue.push_back(chunk);
                        pending_q.paused = true;
                    }
                    Err(e) => {
                        tracing::warn!("DC chunk write error: {e}");
                    }
                }
            }
            offset = end;
        }
    }

    /// Queue a message as chunks for later delivery.
    fn enqueue_chunked(&mut self, data: &[u8], pending: &mut PendingRef) {
        let pending_q = match pending {
            PendingRef::Video => &mut self.video_pending,
            PendingRef::Control => &mut self.control_pending,
        };

        if data.len() <= DC_CHUNK_SIZE {
            pending_q.queue.push_back(data.to_vec());
            return;
        }

        let total = data.len() as u32;
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + DC_CHUNK_SIZE - 4).min(data.len());
            let mut chunk = Vec::with_capacity(4 + (end - offset));
            chunk.extend_from_slice(&total.to_le_bytes());
            chunk.extend_from_slice(&data[offset..end]);
            pending_q.queue.push_back(chunk);
            offset = end;
        }
    }

    /// Try to flush pending chunks for a channel. Stops if write is rejected.
    fn flush_pending(&mut self, channel_id: ChannelId, pending: &mut PendingRef) {
        let pending_q = match pending {
            PendingRef::Video => &mut self.video_pending,
            PendingRef::Control => &mut self.control_pending,
        };

        if pending_q.paused || pending_q.queue.is_empty() {
            return;
        }

        while let Some(chunk) = pending_q.queue.front() {
            if let Some(mut ch) = self.rtc.channel(channel_id) {
                match ch.write(true, chunk) {
                    Ok(true) => {
                        pending_q.queue.pop_front();
                    }
                    Ok(false) => {
                        // Still full — pause again
                        pending_q.paused = true;
                        tracing::trace!(
                            channel = ?channel_id,
                            remaining = pending_q.queue.len(),
                            "DC flush paused (buffer full)"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("DC flush write error: {e}");
                        // Drop the bad chunk and continue
                        pending_q.queue.pop_front();
                    }
                }
            } else {
                // Channel gone
                pending_q.queue.clear();
                return;
            }
        }

        if pending_q.queue.is_empty() {
            tracing::trace!(channel = ?channel_id, "DC pending queue fully drained");
        }
    }
}

/// Helper to select which pending queue to operate on without
/// borrowing all of ActiveClient mutably.
enum PendingRef {
    Video,
    Control,
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
            Message::Hello { .. } | Message::VideoFrame { .. } | Message::TileUpdate { .. } | Message::AudioFrame { .. } => {
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
