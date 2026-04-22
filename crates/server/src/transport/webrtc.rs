use anyhow::{Context, Result};
use phantom_core::encode::EncodedFrame;
use phantom_core::protocol::{AudioCodec, Message};
use phantom_core::transport::{MessageReceiver, MessageSender};
use std::net::UdpSocket;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

mod backend_str0m;
mod backend_phantom;

pub struct PendingRtcSession {
    mode: RtcMode,
    backend_kind: BackendKind,
    backend: BackendPendingSession,
}

pub struct AcceptedRtcSession {
    pub session: PendingRtcSession,
    pub answer_sdp: String,
}

enum BackendPendingSession {
    Str0m(backend_str0m::Str0mPendingRtcSession),
    Phantom(backend_phantom::PhantomPendingRtcSession),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Str0m,
    Phantom,
}

fn default_backend() -> BackendKind {
    match std::env::var("PHANTOM_WEBRTC_BACKEND").ok().as_deref() {
        Some("phantom") => BackendKind::Phantom,
        _ => BackendKind::Str0m,
    }
}

trait BackendClient {
    fn is_alive(&self) -> bool;
    fn should_disconnect(&self) -> bool;
    fn poll_and_flush(
        &mut self,
        socket: &UdpSocket,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    );
    fn drain_outgoing(&mut self);
    fn handle_receive(
        &mut self,
        candidate_addr: std::net::SocketAddr,
        source: std::net::SocketAddr,
        contents: &[u8],
    );
    fn handle_timeout(&mut self);
}

impl PendingRtcSession {
    pub fn mode(&self) -> RtcMode {
        self.mode
    }

    fn backend_kind(&self) -> BackendKind {
        self.backend_kind
    }

    fn into_client(self) -> Box<dyn BackendClient + Send> {
        match self.backend {
            BackendPendingSession::Str0m(session) => {
                Box::new(backend_str0m::Str0mClient::new(session, self.mode))
            }
            BackendPendingSession::Phantom(session) => {
                Box::new(backend_phantom::PhantomClient::new(session, self.mode))
            }
        }
    }
}

pub fn accept_http_offer(
    candidate_addr: std::net::SocketAddr,
    sdp_str: &str,
    mode: RtcMode,
) -> Result<AcceptedRtcSession> {
    let (backend_kind, backend, answer_sdp) = match default_backend() {
        BackendKind::Str0m => {
            let accepted = backend_str0m::accept_http_offer(candidate_addr, sdp_str)?;
            (
                BackendKind::Str0m,
                BackendPendingSession::Str0m(accepted.session),
                accepted.answer_sdp,
            )
        }
        BackendKind::Phantom => {
            let accepted = backend_phantom::accept_http_offer(candidate_addr, sdp_str)?;
            (
                BackendKind::Phantom,
                BackendPendingSession::Phantom(accepted.session),
                accepted.answer_sdp,
            )
        }
    };
    Ok(AcceptedRtcSession {
        session: PendingRtcSession {
            mode,
            backend_kind,
            backend,
        },
        answer_sdp,
    })
}

/// A single WebRTC run loop managing one client at a time.
/// The transport backend currently lives in `backend_str0m`, but the run loop,
/// session bridge, and public sender/receiver types are Phantom-owned so we can
/// swap the backend incrementally.
///
/// Lifecycle:
///   1. Loop waits for Rtc from POST /rtc (via channel)
///   2. Drives ICE/DTLS/SCTP until DataChannels open
///   3. Bridges data between DataChannels and MessageSender/Receiver channels
///   4. When client disconnects, cleans up and goes back to step 1
pub fn run_loop(
    candidate_addr: std::net::SocketAddr,
    rtc_rx: mpsc::Receiver<PendingRtcSession>,
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
    let mut active: Option<Box<dyn BackendClient + Send>> = None;

    loop {
        // 1. Accept new Rtc from POST /rtc.
        //    Drain ALL pending — only keep the latest (browser may have refreshed multiple times).
        {
            let mut latest: Option<PendingRtcSession> = None;
            while let Ok(rtc) = rtc_rx.try_recv() {
                latest = Some(rtc);
            }
            if let Some(session) = latest {
                if active.is_some() {
                    tracing::info!("replacing old client (browser refreshed)");
                }
                tracing::info!(mode = ?session.mode(), backend = ?session.backend_kind(), "new WebRTC client from POST /rtc");
                active = Some(session.into_client());
            }
        }

        // 2. Clean up disconnected client
        if let Some(ref client) = active {
            if !client.is_alive() || client.should_disconnect() {
                tracing::info!(
                    "WebRTC client disconnected (alive={}, disconnecting={})",
                    client.is_alive(),
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
                if let Some(ref mut client) = active {
                    client.handle_receive(candidate_addr, addr, &buf[..n]);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {}
        }

        // 5. Drive time forward
        if let Some(ref mut client) = active {
            client.handle_timeout();
        }
    }
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

#[derive(Clone)]
pub(crate) struct MediaAudioFrame {
    pub(crate) codec: AudioCodec,
    pub(crate) sample_rate: u32,
    pub(crate) data: Vec<u8>,
}

pub(crate) struct SessionBridge {
    pub(crate) sender: WebRtcSender,
    pub(crate) receiver: WebRtcReceiver,
    pub(crate) video_rx: mpsc::Receiver<Vec<u8>>,
    pub(crate) media_video_rx: mpsc::Receiver<EncodedFrame>,
    pub(crate) media_audio_rx: mpsc::Receiver<MediaAudioFrame>,
    pub(crate) control_out_rx: mpsc::Receiver<Vec<u8>>,
    pub(crate) input_in_tx: mpsc::Sender<Vec<u8>>,
    pub(crate) control_in_tx: mpsc::Sender<Vec<u8>>,
}

pub(crate) fn make_session_bridge(mode: RtcMode) -> SessionBridge {
    let (video_tx, video_rx) = mpsc::sync_channel(30);
    let (media_video_tx, media_video_rx) = mpsc::sync_channel(8);
    let (media_audio_tx, media_audio_rx) = mpsc::sync_channel(64);
    let (control_tx, control_out_rx) = mpsc::sync_channel(64);
    let (input_in_tx, input_rx) = mpsc::channel();
    let (control_in_tx, control_rx) = mpsc::channel();

    SessionBridge {
        sender: WebRtcSender {
            mode,
            video_tx,
            media_video_tx,
            media_audio_tx,
            control_tx,
        },
        receiver: WebRtcReceiver {
            input_rx,
            control_rx,
        },
        video_rx,
        media_video_rx,
        media_audio_rx,
        control_out_rx,
        input_in_tx,
        control_in_tx,
    }
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

    #[test]
    fn backend_selection_uses_env_override() {
        unsafe { std::env::remove_var("PHANTOM_WEBRTC_BACKEND") };
        assert_eq!(default_backend(), BackendKind::Str0m);

        unsafe { std::env::set_var("PHANTOM_WEBRTC_BACKEND", "phantom") };
        assert_eq!(default_backend(), BackendKind::Phantom);

        unsafe { std::env::remove_var("PHANTOM_WEBRTC_BACKEND") };
    }
}
