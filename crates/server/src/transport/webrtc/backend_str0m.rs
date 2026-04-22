use super::{
    make_session_bridge, BackendClient, MediaAudioFrame, RtcMode, WebRtcReceiver, WebRtcSender,
};
use anyhow::{Context, Result};
use phantom_core::encode::{EncodedFrame, VideoCodec};
use phantom_core::protocol::AudioCodec;
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
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

pub(super) struct Str0mPendingRtcSession {
    rtc: Rtc,
}

pub(super) struct Str0mAcceptedRtcSession {
    pub session: Str0mPendingRtcSession,
    pub answer_sdp: String,
}

pub(super) fn accept_http_offer(
    candidate_addr: SocketAddr,
    sdp_str: &str,
) -> Result<Str0mAcceptedRtcSession> {
    let mut rtc = Rtc::builder().build(Instant::now());
    let candidate = str0m::Candidate::host(candidate_addr, "udp").context("host candidate")?;
    rtc.add_local_candidate(candidate);

    let offer = str0m::change::SdpOffer::from_sdp_string(sdp_str).context("parse SDP")?;
    let answer = rtc.sdp_api().accept_offer(offer).context("accept offer")?;

    Ok(Str0mAcceptedRtcSession {
        session: Str0mPendingRtcSession { rtc },
        answer_sdp: answer.to_sdp_string(),
    })
}

/// Per-channel pending write queue for backpressure.
struct PendingQueue {
    queue: VecDeque<Vec<u8>>,
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

pub(super) struct Str0mClient {
    rtc: Rtc,
    mode: RtcMode,
    video_id: Option<ChannelId>,
    input_id: Option<ChannelId>,
    control_id: Option<ChannelId>,
    media: MediaTrackState,
    channels_ready: bool,
    ice_disconnected_since: Option<Instant>,
    video_rx: Option<mpsc::Receiver<Vec<u8>>>,
    media_video_rx: Option<mpsc::Receiver<EncodedFrame>>,
    media_audio_rx: Option<mpsc::Receiver<MediaAudioFrame>>,
    control_out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    input_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    control_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    video_pending: PendingQueue,
    control_pending: PendingQueue,
}

impl Str0mClient {
    pub fn new(session: Str0mPendingRtcSession, mode: RtcMode) -> Self {
        let rtc = session.rtc;
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

    fn handle_receive_inner(&mut self, candidate_addr: SocketAddr, source: SocketAddr, contents: &[u8]) {
        let Ok(contents) = contents.try_into() else {
            return;
        };
        let input = Input::Receive(
            Instant::now(),
            str0m::net::Receive {
                proto: Protocol::Udp,
                source,
                destination: candidate_addr,
                contents,
            },
        );
        let _ = self.rtc.handle_input(input);
    }

    fn handle_timeout_inner(&mut self) {
        self.rtc.handle_input(Input::Timeout(Instant::now())).ok();
    }

    fn poll_and_flush_inner(
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

        if let Some(vid) = self.video_id {
            self.flush_pending(vid, &mut PendingRef::Video);
        }
        if let Some(ctrl) = self.control_id {
            self.flush_pending(ctrl, &mut PendingRef::Control);
        }
    }

    fn drain_outgoing_inner(&mut self) {
        if !self.channels_ready {
            return;
        }

        if let Some(vid) = self.video_id {
            self.flush_pending(vid, &mut PendingRef::Video);
        }
        if let Some(ctrl) = self.control_id {
            self.flush_pending(ctrl, &mut PendingRef::Control);
        }

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

                    let bridge = make_session_bridge(self.mode);

                    self.video_rx = Some(bridge.video_rx);
                    self.media_video_rx = Some(bridge.media_video_rx);
                    self.media_audio_rx = Some(bridge.media_audio_rx);
                    self.control_out_rx = Some(bridge.control_out_rx);
                    self.input_in_tx = Some(bridge.input_in_tx);
                    self.control_in_tx = Some(bridge.control_in_tx);

                    *session_slot.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((bridge.sender, bridge.receiver));
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

        if pending_q.paused {
            self.enqueue_chunked(data, pending);
            return;
        }

        if data.len() <= DC_CHUNK_SIZE {
            if let Some(mut ch) = self.rtc.channel(channel_id) {
                match ch.write(true, data) {
                    Ok(true) => {}
                    Ok(false) => {
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
                pending_q.queue.push_back(chunk);
            } else if let Some(mut ch) = self.rtc.channel(channel_id) {
                match ch.write(true, &chunk) {
                    Ok(true) => {}
                    Ok(false) => {
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
                        pending_q.queue.pop_front();
                    }
                }
            } else {
                pending_q.queue.clear();
                return;
            }
        }

        if pending_q.queue.is_empty() {
            tracing::trace!(channel = ?channel_id, "DC pending queue fully drained");
        }
    }
}

impl BackendClient for Str0mClient {
    fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    fn should_disconnect(&self) -> bool {
        self.ice_disconnected_since
            .map(|t| t.elapsed() >= ICE_DISCONNECT_GRACE)
            .unwrap_or(false)
    }

    fn poll_and_flush(
        &mut self,
        socket: &UdpSocket,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        self.poll_and_flush_inner(socket, session_slot, notify_tx);
    }

    fn drain_outgoing(&mut self) {
        self.drain_outgoing_inner();
    }

    fn handle_receive(
        &mut self,
        candidate_addr: SocketAddr,
        source: SocketAddr,
        contents: &[u8],
    ) {
        self.handle_receive_inner(candidate_addr, source, contents);
    }

    fn handle_timeout(&mut self) {
        self.handle_timeout_inner();
    }
}

enum PendingRef {
    Video,
    Control,
}

#[derive(Default)]
struct MediaTrackState {
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    video_rtp_time: u64,
    audio_rtp_time: u64,
}
