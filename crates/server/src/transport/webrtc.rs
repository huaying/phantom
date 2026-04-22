use anyhow::{Context, Result};
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::protocol::{AudioCodec, Message};
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid};
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
/// Don't tear down immediately on transient ICE disconnects.
const ICE_DISCONNECT_GRACE: Duration = Duration::from_secs(5);

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
    rtc_rx: mpsc::Receiver<(Rtc, RtcMode)>,
    session_slot: Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
    notify_tx: mpsc::Sender<()>,
) {
    // One UDP socket for the entire server lifetime
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", candidate_addr.port())) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("bind UDP: {e}");
            return;
        }
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
            let mut latest: Option<(Rtc, RtcMode)> = None;
            while let Ok(rtc) = rtc_rx.try_recv() {
                latest = Some(rtc);
            }
            if let Some((rtc, mode)) = latest {
                if active.is_some() {
                    tracing::info!("replacing old client (browser refreshed)");
                }
                tracing::info!(?mode, "new WebRTC client from POST /rtc");
                active = Some(ActiveClient::new(rtc, mode));
            }
        }

        // 2. Clean up disconnected client
        if let Some(ref client) = active {
            if !client.rtc.is_alive() || client.should_disconnect() {
                tracing::info!(
                    "WebRTC client disconnected (alive={}, disconnecting={})",
                    client.rtc.is_alive(),
                    client.should_disconnect()
                );
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
                let Ok(contents) = (&buf[..n]).try_into() else {
                    continue;
                };
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
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
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
        Self {
            queue: VecDeque::new(),
            paused: false,
        }
    }
}

struct ActiveClient {
    rtc: Rtc,
    mode: RtcMode,
    video_id: Option<ChannelId>,
    input_id: Option<ChannelId>,
    control_id: Option<ChannelId>,
    media: MediaTrackState,
    channels_ready: bool,
    ice_disconnected_since: Option<Instant>,
    /// Session loop sends data here → we write to DataChannels
    video_rx: Option<mpsc::Receiver<Vec<u8>>>,
    media_video_rx: Option<mpsc::Receiver<EncodedFrame>>,
    media_audio_rx: Option<mpsc::Receiver<MediaAudioFrame>>,
    control_out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// We receive data from DataChannels → send to session loop
    input_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    control_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Per-channel backpressure queues
    video_pending: PendingQueue,
    control_pending: PendingQueue,
}

impl ActiveClient {
    fn new(rtc: Rtc, mode: RtcMode) -> Self {
        Self {
            rtc,
            mode,
            video_id: None,
            input_id: None,
            control_id: None,
            media: MediaTrackState::default(),
            channels_ready: false,
            ice_disconnected_since: None,
            video_rx: None,
            media_video_rx: None,
            media_audio_rx: None,
            control_out_rx: None,
            input_in_tx: None,
            control_in_tx: None,
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

                if self.can_start_session() && !self.channels_ready {
                    tracing::info!("all 3 DataChannels open — session ready");
                    self.channels_ready = true;

                    // Bounded for session→runloop (video is high bandwidth)
                    let (video_tx, video_rx) = mpsc::sync_channel(30); // ~1s of frames
                    let (media_video_tx, media_video_rx) = mpsc::sync_channel(8);
                    let (media_audio_tx, media_audio_rx) = mpsc::sync_channel(64);
                    let (ctrl_out_tx, ctrl_out_rx) = mpsc::sync_channel(64);
                    // Unbounded for runloop→session (input/control are small + infrequent)
                    let (input_in_tx, input_in_rx) = mpsc::channel();
                    let (ctrl_in_tx, ctrl_in_rx) = mpsc::channel();

                    self.video_rx = Some(video_rx);
                    self.media_video_rx = Some(media_video_rx);
                    self.media_audio_rx = Some(media_audio_rx);
                    self.control_out_rx = Some(ctrl_out_rx);
                    self.input_in_tx = Some(input_in_tx);
                    self.control_in_tx = Some(ctrl_in_tx);

                    // Overwrite any stale session — main thread always gets the latest
                    *session_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((
                        WebRtcSender {
                            mode: self.mode,
                            video_tx,
                            media_video_tx,
                            media_audio_tx,
                            control_tx: ctrl_out_tx,
                        },
                        WebRtcReceiver {
                            input_rx: input_in_rx,
                            control_rx: ctrl_in_rx,
                        },
                    ));
                    let _ = notify_tx.send(());
                }
            }
            Event::MediaAdded(media) => {
                tracing::info!(
                    kind = ?media.kind,
                    direction = ?media.direction,
                    mid = %media.mid,
                    "WebRTC media added"
                );
                match media.kind {
                    MediaKind::Video => self.media.video_mid = Some(media.mid),
                    MediaKind::Audio => self.media.audio_mid = Some(media.mid),
                }
                if self.can_start_session() && !self.channels_ready {
                    tracing::info!("WebRTC media transceivers negotiated; waiting for channels");
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
                match s {
                    str0m::IceConnectionState::Disconnected => {
                        self.ice_disconnected_since.get_or_insert_with(Instant::now);
                    }
                    _ => {
                        self.ice_disconnected_since = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn should_disconnect(&self) -> bool {
        self.ice_disconnected_since
            .map(|t| t.elapsed() >= ICE_DISCONNECT_GRACE)
            .unwrap_or(false)
    }

    fn can_start_session(&self) -> bool {
        let channels_ready =
            self.video_id.is_some() && self.input_id.is_some() && self.control_id.is_some();
        if !channels_ready {
            return false;
        }
        match self.mode {
            RtcMode::DataChannelV1 => true,
            RtcMode::MediaTracksV1Compat => {
                self.media.video_mid.is_some() && self.media.audio_mid.is_some()
            }
        }
    }

    /// Drain outgoing session data into DataChannels, respecting backpressure.
    fn drain_outgoing(&mut self) {
        if !self.channels_ready {
            return;
        }

        // First: try to flush any pending chunks from previous iterations
        if let Some(vid) = self.video_id {
            self.flush_pending(vid, &mut PendingRef::Video);
        }
        if let Some(ctrl) = self.control_id {
            self.flush_pending(ctrl, &mut PendingRef::Control);
        }

        // Then: pull new messages from session loop
        let video_msgs: Vec<Vec<u8>> = self
            .video_rx
            .as_ref()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        let ctrl_msgs: Vec<Vec<u8>> = self
            .control_out_rx
            .as_ref()
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

        if self.mode == RtcMode::MediaTracksV1Compat {
            let video_frames: Vec<EncodedFrame> = self
                .media_video_rx
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();
            let audio_frames: Vec<MediaAudioFrame> = self
                .media_audio_rx
                .as_ref()
                .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
                .unwrap_or_default();

            for frame in &video_frames {
                self.write_video_track(frame);
            }
            for frame in &audio_frames {
                self.write_audio_track(frame);
            }
        }
    }

    fn write_video_track(&mut self, frame: &EncodedFrame) {
        if frame.codec != VideoCodec::H264 {
            return;
        }
        let Some(mid) = self.media.video_mid else {
            return;
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::H264)
            .map(|p| p.pt())
        else {
            return;
        };
        let rtp_time = MediaTime::from_90khz(self.media.video_rtp_time);
        self.media.video_rtp_time = self.media.video_rtp_time.saturating_add(3_000);
        if let Err(e) = writer.write(pt, Instant::now(), rtp_time, frame.data.clone()) {
            tracing::debug!("WebRTC video track write failed: {e}");
        }
    }

    fn write_audio_track(&mut self, frame: &MediaAudioFrame) {
        if frame.codec != AudioCodec::Opus {
            return;
        }
        let Some(mid) = self.media.audio_mid else {
            return;
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return;
        };
        let rtp_time = MediaTime::new(self.media.audio_rtp_time, Frequency::FORTY_EIGHT_KHZ);
        let step = (frame.sample_rate as u64 / 50).max(1);
        self.media.audio_rtp_time = self.media.audio_rtp_time.saturating_add(step);
        if let Err(e) = writer.write(pt, Instant::now(), rtp_time, frame.data.clone()) {
            tracing::debug!("WebRTC audio track write failed: {e}");
        }
    }

    /// Write data to a DataChannel with proper backpressure handling.
    /// Splits large messages into chunks. If a write is rejected (buffer full),
    /// remaining chunks are queued and we wait for BufferedAmountLow.
    fn write_with_backpressure(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        pending: &mut PendingRef,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcMode {
    DataChannelV1,
    MediaTracksV1Compat,
}

impl RtcMode {
    pub fn from_offer_mode(mode: &str) -> Self {
        match mode {
            "media_tracks_v1_compat" => Self::MediaTracksV1Compat,
            _ => Self::DataChannelV1,
        }
    }
}

#[derive(Default)]
struct MediaTrackState {
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    video_rtp_time: u64,
    audio_rtp_time: u64,
}

#[derive(Clone)]
struct MediaAudioFrame {
    codec: AudioCodec,
    sample_rate: u32,
    data: Vec<u8>,
}

pub struct WebRtcSender {
    mode: RtcMode,
    video_tx: mpsc::SyncSender<Vec<u8>>,
    media_video_tx: mpsc::SyncSender<EncodedFrame>,
    media_audio_tx: mpsc::SyncSender<MediaAudioFrame>,
    control_tx: mpsc::SyncSender<Vec<u8>>,
}

impl MessageSender for WebRtcSender {
    fn send_msg(&mut self, msg: &Message) -> Result<()> {
        match msg {
            Message::VideoFrame { frame, .. } if self.mode == RtcMode::MediaTracksV1Compat => self
                .media_video_tx
                .try_send((**frame).clone())
                .map_err(|e| match e {
                    mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("video track closed"),
                    mpsc::TrySendError::Full(_) => anyhow::anyhow!(""),
                })
                .or(Ok(())),
            Message::AudioFrame {
                codec,
                sample_rate,
                data,
                ..
            } if self.mode == RtcMode::MediaTracksV1Compat => self
                .media_audio_tx
                .try_send(MediaAudioFrame {
                    codec: *codec,
                    sample_rate: *sample_rate,
                    data: data.clone(),
                })
                .map_err(|e| match e {
                    mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("audio track closed"),
                    mpsc::TrySendError::Full(_) => anyhow::anyhow!(""),
                })
                .or(Ok(())),
            Message::Hello { .. } if self.mode == RtcMode::MediaTracksV1Compat => self
                .control_tx
                .try_send(bincode::serialize(msg).context("serialize")?)
                .map_err(|e| match e {
                    mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("control DC closed"),
                    mpsc::TrySendError::Full(_) => anyhow::anyhow!(""),
                })
                .or(Ok(())),
            Message::Hello { .. } | Message::VideoFrame { .. } | Message::AudioFrame { .. } => self
                .video_tx
                .try_send(bincode::serialize(msg).context("serialize")?)
                .map_err(|e| match e {
                    mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("video DC closed"),
                    mpsc::TrySendError::Full(_) => anyhow::anyhow!(""),
                })
                .or(Ok(())),
            _ => self
                .control_tx
                .try_send(bincode::serialize(msg).context("serialize")?)
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

    fn recv_msg_within(&mut self, timeout: Duration) -> Result<Option<Message>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.input_rx.try_recv() {
                Ok(d) => return bincode::deserialize(&d).context("deserialize").map(Some),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("input channel closed");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match self.control_rx.try_recv() {
                Ok(d) => return bincode::deserialize(&d).context("deserialize").map(Some),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("control channel closed");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_core::encode::{EncodedFrame, VideoCodec};
    use phantom_core::frame::PixelFormat;
    use phantom_core::protocol::Message;

    fn make_sender(mode: RtcMode) -> (
        WebRtcSender,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Receiver<EncodedFrame>,
        mpsc::Receiver<MediaAudioFrame>,
        mpsc::Receiver<Vec<u8>>,
    ) {
        let (video_tx, video_rx) = mpsc::sync_channel(8);
        let (media_video_tx, media_video_rx) = mpsc::sync_channel(8);
        let (media_audio_tx, media_audio_rx) = mpsc::sync_channel(8);
        let (control_tx, control_rx) = mpsc::sync_channel(8);
        (
            WebRtcSender {
                mode,
                video_tx,
                media_video_tx,
                media_audio_tx,
                control_tx,
            },
            video_rx,
            media_video_rx,
            media_audio_rx,
            control_rx,
        )
    }

    #[test]
    fn rtc_mode_parsing_defaults_to_datachannel() {
        assert_eq!(RtcMode::from_offer_mode("datachannel_v1"), RtcMode::DataChannelV1);
        assert_eq!(
            RtcMode::from_offer_mode("media_tracks_v1_compat"),
            RtcMode::MediaTracksV1Compat
        );
        assert_eq!(RtcMode::from_offer_mode("unknown_future_mode"), RtcMode::DataChannelV1);
    }

    #[test]
    fn sender_routes_media_track_payloads_in_media_mode() {
        let (mut sender, video_rx, media_video_rx, media_audio_rx, control_rx) =
            make_sender(RtcMode::MediaTracksV1Compat);

        let hello = Message::Hello {
            width: 1280,
            height: 720,
            format: PixelFormat::Bgra8,
            protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
            audio: true,
            video_codec: VideoCodec::H264,
            session_token: vec![],
        };
        sender.send_msg(&hello).unwrap();
        let got: Message = bincode::deserialize(&control_rx.try_recv().unwrap()).unwrap();
        assert!(matches!(got, Message::Hello { .. }));
        assert!(video_rx.try_recv().is_err());

        let frame = EncodedFrame {
            codec: VideoCodec::H264,
            data: vec![0, 0, 0, 1, 0x65, 0x88],
            is_keyframe: true,
        };
        sender
            .send_msg(&Message::VideoFrame {
                sequence: 7,
                frame: Box::new(frame.clone()),
            })
            .unwrap();
        assert_eq!(media_video_rx.try_recv().unwrap().data, frame.data);
        assert!(video_rx.try_recv().is_err());

        sender
            .send_msg(&Message::AudioFrame {
                codec: phantom_core::protocol::AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 2,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        let audio = media_audio_rx.try_recv().unwrap();
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn sender_keeps_legacy_routing_in_datachannel_mode() {
        let (mut sender, video_rx, media_video_rx, media_audio_rx, control_rx) =
            make_sender(RtcMode::DataChannelV1);

        sender
            .send_msg(&Message::RequestKeyframe)
            .unwrap();
        let control_msg: Message = bincode::deserialize(&control_rx.try_recv().unwrap()).unwrap();
        assert!(matches!(control_msg, Message::RequestKeyframe));

        sender
            .send_msg(&Message::Hello {
                width: 800,
                height: 600,
                format: PixelFormat::Bgra8,
                protocol_version: phantom_core::protocol::PROTOCOL_VERSION,
                audio: false,
                video_codec: VideoCodec::H264,
                session_token: vec![],
            })
            .unwrap();
        let video_msg: Message = bincode::deserialize(&video_rx.try_recv().unwrap()).unwrap();
        assert!(matches!(video_msg, Message::Hello { .. }));
        assert!(media_video_rx.try_recv().is_err());
        assert!(media_audio_rx.try_recv().is_err());
    }
}
