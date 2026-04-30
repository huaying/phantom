use super::{
    make_session_bridge,
    sctp::{PhantomSctpStack, SctpNotice},
    BackendClient, MediaAudioFrame, RtcMode, WebRtcReceiver, WebRtcSender,
};
use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit as AesKeyInit};
use aes::{Aes128, Aes256};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce, Tag};
use anyhow::{anyhow, bail, Context, Result};
use dimpl::{
    Config as DtlsConfig, Dtls, DtlsCertificate, KeyingMaterial, Output as DtlsOutput, SrtpProfile,
};
use phantom_core::encode::EncodedFrame;
use phantom_core::protocol::{AudioCodec, Message};
use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256};
use ring::digest;
use ring::hmac;
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use stun_types::attribute::{Username, XorMappedAddress};
use stun_types::message::{
    IntegrityAlgorithm, Message as StunMessage, MessageClass, MessageWrite, MessageWriteExt,
    MessageWriteVec, ShortTermCredentials, BINDING,
};
use uuid::Uuid;

const RTP_HEADER_LEN: usize = 12;
const RTP_MTU: usize = 1200;
const H264_FUA_NALU_TYPE: u8 = 28;
const H264_STAPA_NALU_TYPE: u8 = 24;
const H264_NALU_TYPE_MASK: u8 = 0x1F;
const H264_NALU_REF_IDC_MASK: u8 = 0x60;
const H264_SPS_NALU_TYPE: u8 = 7;
const H264_PPS_NALU_TYPE: u8 = 8;
const H264_IDR_NALU_TYPE: u8 = 5;
const VIDEO_RTX_CACHE_SIZE: usize = 512;
const RTC_MAX_UDP_PACKETS_PER_FLUSH: usize = 32;
const RTC_MAX_VIDEO_FRAMES_PER_DRAIN: usize = 2;
const RTC_STATS_INTERVAL: Duration = Duration::from_secs(5);
// Fallback only. When the browser offer contains H.264 fmtp for the selected
// payload, answer with the offered fmtp so Chrome's decoder path stays on the
// exact negotiated profile. In-band SPS/PPS still carry the real bitstream
// profile/level for each encoder/platform.
const PHANTOM_H264_FMTP_FALLBACK: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42c028";
/// Placeholder for the in-tree Phantom-owned WebRTC backend.
///
/// This module intentionally starts as a compileable skeleton while the public
/// transport API is being decoupled from `str0m`. The goal is to let us
/// replace one backend slice at a time without changing the `/rtc` entrypoint,
/// run loop shape, or the session bridge types again.
pub(super) struct PhantomPendingRtcSession {
    params: PhantomSessionParams,
    dtls: Dtls,
}

pub(super) struct PhantomAcceptedRtcSession {
    pub session: PhantomPendingRtcSession,
    pub answer_sdp: String,
}

pub(super) fn accept_http_offer(
    candidate_addr: SocketAddr,
    sdp_str: &str,
) -> Result<PhantomAcceptedRtcSession> {
    let offer = ParsedOffer::parse(sdp_str)?;
    let params = PhantomSessionParams::derive(candidate_addr, &offer)?;
    let answer_sdp = AnswerBuilder::from_offer(&offer, &params).build();
    let dtls = Dtls::new_auto(
        params.dtls_config.clone(),
        params.local_certificate.clone(),
        Instant::now(),
    );
    tracing::info!(
        video = offer.has_video,
        audio = offer.has_audio,
        application = offer.has_application,
        candidates = offer.candidate_count,
        mids = ?offer.mids,
        answer_len = answer_sdp.len(),
        backend_candidate = %params.candidate_addr,
        remote_ice_ufrag = %params.remote_ice_ufrag,
        "phantom backend parsed SDP offer"
    );
    Ok(PhantomAcceptedRtcSession {
        session: PhantomPendingRtcSession { params, dtls },
        answer_sdp,
    })
}

pub(super) struct PhantomClient {
    mode: RtcMode,
    params: PhantomSessionParams,
    dtls: Dtls,
    out_buf: Vec<u8>,
    last_source: Option<SocketAddr>,
    alive: bool,
    disconnecting: bool,
    connected: bool,
    pending_transmits: VecDeque<(SocketAddr, Vec<u8>)>,
    sctp: PhantomSctpStack,
    channels_ready: bool,
    input_stream: Option<u16>,
    control_stream: Option<u16>,
    media_video_rx: Option<mpsc::Receiver<EncodedFrame>>,
    media_audio_rx: Option<mpsc::Receiver<MediaAudioFrame>>,
    control_out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    input_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    control_in_tx: Option<mpsc::Sender<Vec<u8>>>,
    srtp_tx: Option<PhantomSrtpTxContext>,
    srtp_rx: Option<PhantomSrtpRxContext>,
    media_tx: MediaTxState,
    last_rtcp_report_at: Instant,
    stats: RtcStatsWindow,
    video_rtx_cache: VecDeque<CachedRtpPacket>,
    logged_first_packet: bool,
    logged_first_stun: bool,
    logged_first_dtls: bool,
    logged_first_rtcp: bool,
    logged_first_rtp: bool,
}

impl PhantomClient {
    pub fn new(session: PhantomPendingRtcSession, mode: RtcMode) -> Self {
        tracing::info!(
            candidate = %session.params.candidate_addr,
            ice_ufrag = %session.params.ice_ufrag,
            fingerprint = %session.params.fingerprint_sha256,
            "phantom backend session created"
        );
        Self {
            mode,
            params: session.params,
            dtls: session.dtls,
            out_buf: vec![0u8; 2048],
            last_source: None,
            alive: true,
            disconnecting: false,
            connected: false,
            pending_transmits: VecDeque::new(),
            sctp: PhantomSctpStack::new(),
            channels_ready: false,
            input_stream: None,
            control_stream: None,
            media_video_rx: None,
            media_audio_rx: None,
            control_out_rx: None,
            input_in_tx: None,
            control_in_tx: None,
            srtp_tx: None,
            srtp_rx: None,
            media_tx: MediaTxState::new(),
            last_rtcp_report_at: Instant::now(),
            stats: RtcStatsWindow::new(),
            video_rtx_cache: VecDeque::with_capacity(VIDEO_RTX_CACHE_SIZE),
            logged_first_packet: false,
            logged_first_stun: false,
            logged_first_dtls: false,
            logged_first_rtcp: false,
            logged_first_rtp: false,
        }
    }

    fn is_stun_packet(packet: &[u8]) -> bool {
        packet.len() >= 20
            && (packet[0] & 0b1100_0000) == 0
            && packet[4..8] == [0x21, 0x12, 0xA4, 0x42]
    }

    fn expected_stun_username(&self) -> String {
        format!("{}:{}", self.params.ice_ufrag, self.params.remote_ice_ufrag)
    }

    fn handle_sctp_dtls_payload(&mut self, source: SocketAddr, payload: &[u8]) {
        let now = Instant::now();
        let _ = self.sctp.handle_dtls_payload(now, source, payload);
    }

    fn maybe_publish_session(
        &mut self,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        if self.channels_ready {
            return;
        }
        let channels_ready = self.input_stream.is_some() && self.control_stream.is_some();
        if !channels_ready {
            return;
        }

        let bridge = make_session_bridge(self.mode);
        let sender = bridge.sender;
        let receiver = bridge.receiver;
        self.media_video_rx = Some(bridge.media_video_rx);
        self.media_audio_rx = Some(bridge.media_audio_rx);
        self.control_out_rx = Some(bridge.control_out_rx);
        self.input_in_tx = Some(bridge.input_in_tx);
        self.control_in_tx = Some(bridge.control_in_tx);
        *session_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((sender, receiver));
        let _ = notify_tx.send(());
        self.channels_ready = true;
        tracing::info!(
            input = ?self.input_stream,
            control = ?self.control_stream,
            "phantom backend DataChannels ready"
        );
    }

    fn handle_datachannel_open(
        &mut self,
        stream_id: u16,
        label: &str,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        tracing::info!(stream_id, label = %label, "phantom backend DataChannel opened");
        match label {
            "input" => self.input_stream = Some(stream_id),
            "control" => self.control_stream = Some(stream_id),
            _ => {}
        }
        self.maybe_publish_session(session_slot, notify_tx);
    }

    fn handle_stream_payload(&mut self, stream_id: u16, payload: &[u8]) {
        if Some(stream_id) == self.input_stream {
            if let Some(tx) = &self.input_in_tx {
                let _ = tx.send(payload.to_vec());
            }
        } else if Some(stream_id) == self.control_stream {
            if let Some(tx) = &self.control_in_tx {
                let _ = tx.send(payload.to_vec());
            }
        }
    }

    fn poll_sctp(
        &mut self,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        let now = Instant::now();
        for event in self.sctp.poll(now) {
            match event {
                SctpNotice::Connected => {
                    tracing::info!("phantom backend SCTP connected");
                }
                SctpNotice::DataChannelOpened { stream_id, label } => {
                    self.handle_datachannel_open(stream_id, &label, session_slot, notify_tx);
                }
                SctpNotice::DataChannelData { stream_id, payload } => {
                    self.handle_stream_payload(stream_id, &payload);
                }
                SctpNotice::HandshakeFailed(reason) => {
                    self.disconnecting = true;
                    tracing::warn!(%reason, "phantom backend SCTP handshake failed");
                }
                SctpNotice::AssociationLost { reason, id } => {
                    tracing::warn!(%reason, id, "phantom backend SCTP association lost");
                }
            }
        }
        self.sctp.drain_transmits(now, |chunk| {
            let _ = self.dtls.send_application_data(chunk);
        });
    }

    fn send_video_frame(&mut self, frame: &EncodedFrame) {
        let Some(source) = self.last_source else {
            return;
        };
        let video_timestamp = self.media_tx.video_timestamp_now();
        let mut payloads = self
            .media_tx
            .h264
            .packetize(RTP_MTU.saturating_sub(RTP_HEADER_LEN + 16), &frame.data);
        let packet_count = payloads.len();
        let mut octet_count = 0u32;
        let mut cached_packets = Vec::with_capacity(packet_count);
        let Some(srtp) = self.srtp_tx.as_mut() else {
            return;
        };
        for (i, payload) in payloads.drain(..).enumerate() {
            let marker = i + 1 == packet_count;
            octet_count = octet_count.wrapping_add(payload.len() as u32);
            let sequence_number = self.media_tx.video_seq;
            let packet = srtp.protect_rtp(
                self.params.video_payload_type,
                marker,
                sequence_number,
                video_timestamp,
                self.media_tx.video_ssrc,
                self.params.video_mid_ext_id,
                self.params.video_mid.as_deref(),
                &payload,
            );
            cached_packets.push((sequence_number, packet.clone()));
            self.pending_transmits.push_back((source, packet));
            self.media_tx.video_seq = self.media_tx.video_seq.wrapping_add(1);
        }
        for (sequence_number, packet) in cached_packets {
            self.cache_video_packet(sequence_number, packet);
        }
        self.media_tx.video_packet_count = self
            .media_tx
            .video_packet_count
            .wrapping_add(packet_count as u32);
        self.media_tx.video_octet_count = self.media_tx.video_octet_count.wrapping_add(octet_count);
        self.media_tx.video_last_rtp_timestamp = video_timestamp;
        self.stats
            .note_video_frame(frame.is_keyframe, packet_count as u64, octet_count as u64);
    }

    fn send_audio_frame(&mut self, frame: &MediaAudioFrame) {
        if !matches!(frame.codec, AudioCodec::Opus) {
            return;
        }
        let Some(source) = self.last_source else {
            return;
        };
        let Some(srtp) = self.srtp_tx.as_mut() else {
            return;
        };
        let packet = srtp.protect_rtp(
            self.params.audio_payload_type,
            false,
            self.media_tx.audio_seq,
            self.media_tx.audio_timestamp,
            self.media_tx.audio_ssrc,
            self.params.audio_mid_ext_id,
            self.params.audio_mid.as_deref(),
            &frame.data,
        );
        self.pending_transmits.push_back((source, packet));
        self.media_tx.audio_seq = self.media_tx.audio_seq.wrapping_add(1);
        self.media_tx.audio_packet_count = self.media_tx.audio_packet_count.wrapping_add(1);
        self.media_tx.audio_octet_count = self
            .media_tx
            .audio_octet_count
            .wrapping_add(frame.data.len() as u32);
        self.stats.note_audio_packet(frame.data.len() as u64);
        self.media_tx.audio_last_rtp_timestamp = self.media_tx.audio_timestamp;
        self.media_tx.audio_timestamp = self
            .media_tx
            .audio_timestamp
            .wrapping_add((frame.sample_rate / 50).max(1));
    }

    fn maybe_send_sender_reports(&mut self) {
        let Some(source) = self.last_source else {
            return;
        };
        let Some(srtp) = self.srtp_tx.as_mut() else {
            return;
        };
        if self.last_rtcp_report_at.elapsed().as_millis() < 1000 {
            return;
        }
        self.last_rtcp_report_at = Instant::now();

        let (ntp_secs, ntp_frac) = current_ntp_timestamp();
        if self.media_tx.video_packet_count != 0 {
            let sr = build_rtcp_sender_report(
                self.media_tx.video_ssrc,
                ntp_secs,
                ntp_frac,
                self.media_tx.video_last_rtp_timestamp,
                self.media_tx.video_packet_count,
                self.media_tx.video_octet_count,
            );
            let packet = srtp.protect_rtcp(
                &sr,
                self.media_tx.video_ssrc,
                self.media_tx.video_srtcp_index,
            );
            self.pending_transmits.push_back((source, packet));
            self.media_tx.video_srtcp_index = self.media_tx.video_srtcp_index.wrapping_add(1);
        }
        if self.media_tx.audio_packet_count != 0 {
            let sr = build_rtcp_sender_report(
                self.media_tx.audio_ssrc,
                ntp_secs,
                ntp_frac,
                self.media_tx.audio_last_rtp_timestamp,
                self.media_tx.audio_packet_count,
                self.media_tx.audio_octet_count,
            );
            let packet = srtp.protect_rtcp(
                &sr,
                self.media_tx.audio_ssrc,
                self.media_tx.audio_srtcp_index,
            );
            self.pending_transmits.push_back((source, packet));
            self.media_tx.audio_srtcp_index = self.media_tx.audio_srtcp_index.wrapping_add(1);
        }
    }

    fn cache_video_packet(&mut self, sequence_number: u16, packet: Vec<u8>) {
        if self.video_rtx_cache.len() >= VIDEO_RTX_CACHE_SIZE {
            self.video_rtx_cache.pop_front();
        }
        self.video_rtx_cache.push_back(CachedRtpPacket {
            sequence_number,
            packet,
        });
    }

    fn retransmit_nacked_video(&mut self, nack_sequences: &[u16]) {
        let Some(source) = self.last_source else {
            return;
        };
        let mut resent = 0usize;
        let mut resent_bytes = 0usize;
        let mut packets = Vec::new();
        for seq in nack_sequences {
            if let Some(pkt) = self
                .video_rtx_cache
                .iter()
                .rev()
                .find(|pkt| pkt.sequence_number == *seq)
            {
                resent_bytes += pkt.packet.len();
                packets.push(pkt.packet.clone());
                resent += 1;
            }
        }
        for packet in packets.into_iter().rev() {
            self.pending_transmits.push_front((source, packet));
        }
        if resent != 0 {
            self.stats.note_rtx(resent as u64, resent_bytes as u64);
            tracing::debug!(
                count = resent,
                "phantom backend retransmitted NACKed RTP packets"
            );
        }
    }
}

impl BackendClient for PhantomClient {
    fn is_alive(&self) -> bool {
        self.alive
    }

    fn should_disconnect(&self) -> bool {
        self.disconnecting
    }

    fn poll_and_flush(
        &mut self,
        socket: &UdpSocket,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        let pending_len = self.pending_transmits.len();
        self.stats.note_pending_udp(pending_len);
        let packets_to_send = pending_len.min(RTC_MAX_UDP_PACKETS_PER_FLUSH);
        self.stats.note_udp_burst(packets_to_send);
        for _ in 0..packets_to_send {
            let Some((addr, packet)) = self.pending_transmits.pop_front() else {
                break;
            };
            self.stats.note_udp_packet(packet.len() as u64);
            if let Err(error) = socket.send_to(&packet, addr) {
                tracing::warn!(%addr, len = packet.len(), %error, "phantom backend failed to send UDP packet");
            }
        }
        loop {
            match self.dtls.poll_output(&mut self.out_buf) {
                DtlsOutput::Packet(packet) => {
                    if let Some(addr) = self.last_source {
                        self.stats.note_udp_packet(packet.len() as u64);
                        self.stats.note_dtls_packet(packet.len() as u64);
                        if let Err(error) = socket.send_to(packet, addr) {
                            tracing::warn!(%addr, len = packet.len(), %error, "phantom backend failed to send DTLS packet");
                        }
                    }
                }
                DtlsOutput::Timeout(_) => break,
                DtlsOutput::Connected => {
                    if !self.connected {
                        self.connected = true;
                        tracing::info!("phantom backend DTLS connected");
                    }
                }
                DtlsOutput::PeerCert(cert_der) => {
                    let actual = format_fingerprint(&calculate_fingerprint(cert_der));
                    let expected = &self.params.remote_fingerprint_sha256;
                    if actual != *expected {
                        self.disconnecting = true;
                        tracing::warn!(
                            expected = %expected,
                            actual = %actual,
                            "phantom backend DTLS peer fingerprint mismatch"
                        );
                    }
                }
                DtlsOutput::KeyingMaterial(material, profile) => {
                    match (
                        PhantomSrtpTxContext::new(profile, &material, false),
                        PhantomSrtpRxContext::new(profile, &material, true),
                    ) {
                        (Ok(tx), Ok(rx)) => {
                            self.srtp_tx = Some(tx);
                            self.srtp_rx = Some(rx);
                            tracing::info!(?profile, "phantom backend exported DTLS-SRTP material");
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            self.disconnecting = true;
                            tracing::warn!(%error, ?profile, "phantom backend failed to initialize SRTP");
                        }
                    }
                }
                DtlsOutput::ApplicationData(data) => {
                    if let Some(source) = self.last_source {
                        let payload = data.to_vec();
                        self.handle_sctp_dtls_payload(source, &payload);
                    }
                }
                _ => {}
            }
        }
        self.stats.maybe_log();
        self.poll_sctp(session_slot, notify_tx);
    }

    fn drain_outgoing(&mut self) {
        let mut control_msgs = Vec::new();
        let mut video_frames = Vec::new();
        let mut audio_frames = Vec::new();
        if let Some(rx) = &self.control_out_rx {
            control_msgs.extend(std::iter::from_fn(|| rx.try_recv().ok()));
        }
        if let Some(rx) = &self.media_video_rx {
            while video_frames.len() < RTC_MAX_VIDEO_FRAMES_PER_DRAIN {
                match rx.try_recv() {
                    Ok(frame) => video_frames.push(frame),
                    Err(_) => break,
                }
            }
        }
        self.stats.note_video_drain(video_frames.len());
        if let Some(rx) = &self.media_audio_rx {
            audio_frames.extend(std::iter::from_fn(|| rx.try_recv().ok()));
        }
        let Some(stream_id) = self.control_stream else {
            return;
        };
        for msg in control_msgs {
            self.sctp.send_binary(stream_id, &msg);
        }
        for frame in &audio_frames {
            self.send_audio_frame(frame);
        }
        for frame in &video_frames {
            self.send_video_frame(frame);
        }
        self.maybe_send_sender_reports();
    }

    fn handle_receive(&mut self, _candidate_addr: SocketAddr, source: SocketAddr, contents: &[u8]) {
        self.last_source = Some(source);
        if !self.logged_first_packet {
            self.logged_first_packet = true;
            tracing::info!(
                source = %source,
                len = contents.len(),
                first_byte = contents.first().copied().unwrap_or_default(),
                "phantom backend received first UDP packet"
            );
        }
        if let Some(request) = parse_stun_binding_request(contents) {
            if !self.logged_first_stun {
                self.logged_first_stun = true;
                tracing::info!(
                    source = %source,
                    username = ?request.username,
                    expected = %self.expected_stun_username(),
                    "phantom backend received STUN binding request"
                );
            }
            if request.username.as_deref() == Some(self.expected_stun_username().as_str()) {
                if let Some(response) = build_stun_success_response(contents, source, &self.params)
                {
                    self.pending_transmits.push_back((source, response));
                    tracing::debug!(source = %source, "phantom backend queued STUN success response");
                } else {
                    tracing::warn!(source = %source, "phantom backend failed to build STUN success response");
                }
            } else {
                tracing::debug!(
                    username = ?request.username,
                    expected = %self.expected_stun_username(),
                    "phantom backend ignored STUN request with unexpected username"
                );
            }
            return;
        }
        if Self::is_stun_packet(contents) {
            return;
        }
        if is_rtcp_packet(contents) {
            if !self.logged_first_rtcp {
                self.logged_first_rtcp = true;
                tracing::info!(source = %source, len = contents.len(), "phantom backend received RTCP/SRTCP packet");
            }
            if let Some(srtp) = self.srtp_rx.as_mut() {
                if let Some(rtcp) = srtp.unprotect_rtcp(contents) {
                    let feedback = parse_rtcp_feedback(&rtcp);
                    if feedback.requests_keyframe {
                        self.stats.note_pli();
                        tracing::debug!("phantom backend received RTCP PLI/FIR");
                        if let Some(tx) = &self.control_in_tx {
                            let _ = tx.send(
                                bincode::serialize(&Message::RequestKeyframe).unwrap_or_default(),
                            );
                        }
                    }
                    if !feedback.nack_sequences.is_empty() {
                        self.stats.note_nacks(feedback.nack_sequences.len() as u64);
                        self.retransmit_nacked_video(&feedback.nack_sequences);
                    }
                    if let Some(report) = feedback.receiver_report {
                        self.stats.note_receiver_report(report);
                        tracing::trace!(
                            fraction_lost = report.fraction_lost,
                            jitter = report.jitter,
                            "phantom backend received RTCP receiver report"
                        );
                    }
                }
            }
            return;
        }
        if is_rtp_packet(contents) {
            if !self.logged_first_rtp {
                self.logged_first_rtp = true;
                tracing::info!(source = %source, len = contents.len(), "phantom backend received RTP/SRTP packet");
            }
            return;
        }
        if !self.logged_first_dtls {
            self.logged_first_dtls = true;
            tracing::info!(
                source = %source,
                len = contents.len(),
                first_byte = contents.first().copied().unwrap_or_default(),
                "phantom backend received DTLS-or-other packet"
            );
        }
        if let Err(error) = self.dtls.handle_packet(contents) {
            tracing::debug!(%error, "phantom backend DTLS packet rejected");
        }
    }

    fn handle_timeout(&mut self) {
        if let Err(error) = self.dtls.handle_timeout(Instant::now()) {
            tracing::debug!(%error, "phantom backend DTLS timeout step failed");
        }
    }
}

fn is_rtp_packet(packet: &[u8]) -> bool {
    packet.len() >= 12
        && packet
            .first()
            .map(|b| (0x80..=0xBF).contains(b))
            .unwrap_or(false)
        && packet.get(1).map(|b| *b < 192 || *b > 223).unwrap_or(false)
}

fn is_rtcp_packet(packet: &[u8]) -> bool {
    packet.len() >= 8
        && packet
            .first()
            .map(|b| (0x80..=0xBF).contains(b))
            .unwrap_or(false)
        && packet
            .get(1)
            .map(|b| (192..=223).contains(b))
            .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct MediaTxState {
    video_ssrc: u32,
    audio_ssrc: u32,
    video_seq: u16,
    audio_seq: u16,
    video_timestamp: u32,
    video_clock_started_at: Instant,
    audio_timestamp: u32,
    video_last_rtp_timestamp: u32,
    audio_last_rtp_timestamp: u32,
    video_packet_count: u32,
    audio_packet_count: u32,
    video_octet_count: u32,
    audio_octet_count: u32,
    video_srtcp_index: u32,
    audio_srtcp_index: u32,
    h264: H264PacketizerState,
}

impl MediaTxState {
    fn new() -> Self {
        let seed = Uuid::new_v4().as_u128();
        Self {
            video_ssrc: (seed as u32).max(1),
            audio_ssrc: ((seed >> 32) as u32).max(1),
            video_seq: (seed >> 64) as u16,
            audio_seq: (seed >> 80) as u16,
            video_timestamp: (seed >> 96) as u32,
            video_clock_started_at: Instant::now(),
            audio_timestamp: (seed >> 64) as u32,
            video_last_rtp_timestamp: (seed >> 96) as u32,
            audio_last_rtp_timestamp: (seed >> 64) as u32,
            video_packet_count: 0,
            audio_packet_count: 0,
            video_octet_count: 0,
            audio_octet_count: 0,
            video_srtcp_index: 0,
            audio_srtcp_index: 0,
            h264: H264PacketizerState::default(),
        }
    }

    fn video_timestamp_now(&mut self) -> u32 {
        let elapsed_ticks =
            (self.video_clock_started_at.elapsed().as_micros() as u64).saturating_mul(90) / 1000;
        let timestamp = self.video_timestamp.wrapping_add(elapsed_ticks as u32);
        // A single backend drain can flush several queued desktop frames faster
        // than the 90 kHz RTP clock advances. Keep timestamps strictly
        // increasing so browser jitter buffers never see a tiny backwards step.
        if timestamp == self.video_last_rtp_timestamp
            || timestamp.wrapping_sub(self.video_last_rtp_timestamp) > 0x8000_0000
        {
            return self.video_last_rtp_timestamp.wrapping_add(1);
        }
        timestamp
    }
}

#[derive(Debug, Default, Clone)]
struct H264PacketizerState {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct CachedRtpPacket {
    sequence_number: u16,
    packet: Vec<u8>,
}

#[derive(Debug, Default)]
struct RtcpFeedback {
    requests_keyframe: bool,
    nack_sequences: Vec<u16>,
    receiver_report: Option<ReceiverReportSummary>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReceiverReportSummary {
    fraction_lost: u8,
    jitter: u32,
}

#[derive(Debug)]
struct RtcStatsWindow {
    started_at: Instant,
    video_frames: u64,
    video_keyframes: u64,
    video_payload_bytes: u64,
    video_packets: u64,
    audio_packets: u64,
    audio_payload_bytes: u64,
    udp_packets: u64,
    udp_bytes: u64,
    dtls_packets: u64,
    dtls_bytes: u64,
    nack_sequences: u64,
    pli_requests: u64,
    rtx_packets: u64,
    rtx_bytes: u64,
    rr_count: u64,
    rr_fraction_lost_max: u8,
    rr_jitter_max: u32,
    max_video_drain: usize,
    max_pending_udp: usize,
    max_udp_burst: usize,
}

impl RtcStatsWindow {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            video_frames: 0,
            video_keyframes: 0,
            video_payload_bytes: 0,
            video_packets: 0,
            audio_packets: 0,
            audio_payload_bytes: 0,
            udp_packets: 0,
            udp_bytes: 0,
            dtls_packets: 0,
            dtls_bytes: 0,
            nack_sequences: 0,
            pli_requests: 0,
            rtx_packets: 0,
            rtx_bytes: 0,
            rr_count: 0,
            rr_fraction_lost_max: 0,
            rr_jitter_max: 0,
            max_video_drain: 0,
            max_pending_udp: 0,
            max_udp_burst: 0,
        }
    }

    fn note_video_frame(&mut self, keyframe: bool, packets: u64, payload_bytes: u64) {
        self.video_frames += 1;
        self.video_keyframes += u64::from(keyframe);
        self.video_packets += packets;
        self.video_payload_bytes += payload_bytes;
    }

    fn note_audio_packet(&mut self, payload_bytes: u64) {
        self.audio_packets += 1;
        self.audio_payload_bytes += payload_bytes;
    }

    fn note_udp_packet(&mut self, bytes: u64) {
        self.udp_packets += 1;
        self.udp_bytes += bytes;
    }

    fn note_dtls_packet(&mut self, bytes: u64) {
        self.dtls_packets += 1;
        self.dtls_bytes += bytes;
    }

    fn note_nacks(&mut self, sequences: u64) {
        self.nack_sequences += sequences;
    }

    fn note_pli(&mut self) {
        self.pli_requests += 1;
    }

    fn note_rtx(&mut self, packets: u64, bytes: u64) {
        self.rtx_packets += packets;
        self.rtx_bytes += bytes;
    }

    fn note_receiver_report(&mut self, report: ReceiverReportSummary) {
        self.rr_count += 1;
        self.rr_fraction_lost_max = self.rr_fraction_lost_max.max(report.fraction_lost);
        self.rr_jitter_max = self.rr_jitter_max.max(report.jitter);
    }

    fn note_video_drain(&mut self, frames: usize) {
        self.max_video_drain = self.max_video_drain.max(frames);
    }

    fn note_pending_udp(&mut self, packets: usize) {
        self.max_pending_udp = self.max_pending_udp.max(packets);
    }

    fn note_udp_burst(&mut self, packets: usize) {
        self.max_udp_burst = self.max_udp_burst.max(packets);
    }

    fn maybe_log(&mut self) {
        let elapsed = self.started_at.elapsed();
        if elapsed < RTC_STATS_INTERVAL {
            return;
        }
        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let video_fps = self.video_frames as f64 / elapsed_secs;
        let video_kbps = (self.video_payload_bytes as f64 * 8.0) / elapsed_secs / 1000.0;
        let udp_kbps = (self.udp_bytes as f64 * 8.0) / elapsed_secs / 1000.0;
        let dtls_kbps = (self.dtls_bytes as f64 * 8.0) / elapsed_secs / 1000.0;
        tracing::info!(
            elapsed_ms = elapsed.as_millis() as u64,
            video_frames = self.video_frames,
            video_fps = format_args!("{video_fps:.1}"),
            video_keyframes = self.video_keyframes,
            video_packets = self.video_packets,
            video_kbps = format_args!("{video_kbps:.1}"),
            audio_packets = self.audio_packets,
            udp_packets = self.udp_packets,
            udp_kbps = format_args!("{udp_kbps:.1}"),
            dtls_packets = self.dtls_packets,
            dtls_kbps = format_args!("{dtls_kbps:.1}"),
            nack_sequences = self.nack_sequences,
            pli_requests = self.pli_requests,
            rtx_packets = self.rtx_packets,
            rtx_kb = self.rtx_bytes / 1024,
            rr_count = self.rr_count,
            rr_fraction_lost_max = self.rr_fraction_lost_max,
            rr_jitter_max = self.rr_jitter_max,
            max_video_drain = self.max_video_drain,
            max_pending_udp = self.max_pending_udp,
            max_udp_burst = self.max_udp_burst,
            "rtc-stats"
        );
        *self = Self::new();
    }
}

struct PhantomSrtpTxContext {
    rtp: PhantomSrtpCipher,
}

struct PhantomSrtpRxContext {
    rtcp: PhantomSrtpCipher,
}

enum PhantomSrtpCipher {
    AeadAes128Gcm {
        key: Aes128Gcm,
        salt: [u8; 12],
    },
    AeadAes256Gcm {
        key: Aes256Gcm,
        salt: [u8; 12],
    },
    Aes128CmSha1_80 {
        enc_key: [u8; 16],
        auth_key: [u8; 20],
        salt: [u8; 14],
    },
}

impl PhantomSrtpTxContext {
    fn new(profile: SrtpProfile, material: &KeyingMaterial, active: bool) -> Result<Self> {
        let left = active;
        let rtp = match profile {
            SrtpProfile::AEAD_AES_128_GCM => {
                let (key, salt) = derive_gcm_material_128(material, left)?;
                PhantomSrtpCipher::AeadAes128Gcm {
                    key: Aes128Gcm::new_from_slice(&key)
                        .map_err(|_| anyhow!("invalid AES-128-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AEAD_AES_256_GCM => {
                let (key, salt) = derive_gcm_material_256(material, left)?;
                PhantomSrtpCipher::AeadAes256Gcm {
                    key: Aes256Gcm::new_from_slice(&key)
                        .map_err(|_| anyhow!("invalid AES-256-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AES128_CM_SHA1_80 => {
                let (enc_key, auth_key, salt) = derive_cm_material_128(material, left)?;
                PhantomSrtpCipher::Aes128CmSha1_80 {
                    enc_key,
                    auth_key,
                    salt,
                }
            }
            _ => {
                bail!("unsupported SRTP profile {}", profile);
            }
        };
        Ok(Self { rtp })
    }

    fn protect_rtp(
        &mut self,
        payload_type: u8,
        marker: bool,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        mid_ext_id: Option<u8>,
        mid: Option<&str>,
        payload: &[u8],
    ) -> Vec<u8> {
        let header = build_rtp_header(
            payload_type,
            marker,
            sequence_number,
            timestamp,
            ssrc,
            mid_ext_id,
            mid,
        );
        match &mut self.rtp {
            PhantomSrtpCipher::AeadAes128Gcm { key, salt } => protect_rtp_gcm(
                key,
                *salt,
                &header,
                sequence_number,
                timestamp,
                ssrc,
                payload,
            ),
            PhantomSrtpCipher::AeadAes256Gcm { key, salt } => protect_rtp_gcm(
                key,
                *salt,
                &header,
                sequence_number,
                timestamp,
                ssrc,
                payload,
            ),
            PhantomSrtpCipher::Aes128CmSha1_80 {
                enc_key,
                auth_key,
                salt,
            } => protect_rtp_aes_cm_sha1_80(
                enc_key,
                auth_key,
                *salt,
                &header,
                sequence_number,
                ssrc,
                payload,
            ),
        }
    }

    fn protect_rtcp(&mut self, packet: &[u8], ssrc: u32, srtcp_index: u32) -> Vec<u8> {
        match &mut self.rtp {
            PhantomSrtpCipher::AeadAes128Gcm { key, salt } => {
                protect_rtcp_gcm(key, *salt, packet, ssrc, srtcp_index)
            }
            PhantomSrtpCipher::AeadAes256Gcm { key, salt } => {
                protect_rtcp_gcm(key, *salt, packet, ssrc, srtcp_index)
            }
            PhantomSrtpCipher::Aes128CmSha1_80 { auth_key, .. } => {
                protect_rtcp_aes_cm_sha1_80(auth_key, packet, srtcp_index)
            }
        }
    }
}

impl PhantomSrtpRxContext {
    fn new(profile: SrtpProfile, material: &KeyingMaterial, active: bool) -> Result<Self> {
        let left = active;
        let rtcp = match profile {
            SrtpProfile::AEAD_AES_128_GCM => {
                let (key, salt) = derive_gcm_material_128(material, left)?;
                PhantomSrtpCipher::AeadAes128Gcm {
                    key: Aes128Gcm::new_from_slice(&key)
                        .map_err(|_| anyhow!("invalid AES-128-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AEAD_AES_256_GCM => {
                let (key, salt) = derive_gcm_material_256(material, left)?;
                PhantomSrtpCipher::AeadAes256Gcm {
                    key: Aes256Gcm::new_from_slice(&key)
                        .map_err(|_| anyhow!("invalid AES-256-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AES128_CM_SHA1_80 => {
                let (enc_key, auth_key, salt) = derive_cm_material_128(material, left)?;
                PhantomSrtpCipher::Aes128CmSha1_80 {
                    enc_key,
                    auth_key,
                    salt,
                }
            }
            _ => {
                bail!("unsupported SRTP profile {}", profile);
            }
        };
        Ok(Self { rtcp })
    }

    fn unprotect_rtcp(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        match &mut self.rtcp {
            PhantomSrtpCipher::AeadAes128Gcm { key, salt } => {
                unprotect_rtcp_gcm(key, *salt, packet)
            }
            PhantomSrtpCipher::AeadAes256Gcm { key, salt } => {
                unprotect_rtcp_gcm(key, *salt, packet)
            }
            PhantomSrtpCipher::Aes128CmSha1_80 {
                enc_key,
                auth_key,
                salt,
            } => unprotect_rtcp_aes_cm_sha1_80(enc_key, auth_key, *salt, packet),
        }
    }
}

fn build_rtp_header(
    payload_type: u8,
    marker: bool,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    mid_ext_id: Option<u8>,
    mid: Option<&str>,
) -> Vec<u8> {
    let mut header = vec![0u8; RTP_HEADER_LEN];
    header[0] = 0x80;
    header[1] = (if marker { 0x80 } else { 0x00 }) | (payload_type & 0x7F);
    header[2..4].copy_from_slice(&sequence_number.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    if let (Some(ext_id), Some(mid_value)) = (mid_ext_id, mid) {
        if (1..=14).contains(&ext_id) && !mid_value.is_empty() && mid_value.len() <= 16 {
            let mut ext = Vec::new();
            ext.push((ext_id << 4) | ((mid_value.len() as u8 - 1) & 0x0F));
            ext.extend_from_slice(mid_value.as_bytes());
            while ext.len() % 4 != 0 {
                ext.push(0);
            }
            header[0] |= 0x10;
            header.extend_from_slice(&0xBEDEu16.to_be_bytes());
            header.extend_from_slice(&((ext.len() / 4) as u16).to_be_bytes());
            header.extend_from_slice(&ext);
        }
    }
    header
}

fn protect_rtp_gcm<C>(
    key: &C,
    salt: [u8; 12],
    header: &[u8],
    sequence_number: u16,
    _timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8>
where
    C: AeadInPlace,
{
    let iv = rtp_gcm_iv(salt, ssrc, 0, sequence_number);
    let nonce = Nonce::from_slice(&iv);
    let mut body = payload.to_vec();
    let tag = key
        .encrypt_in_place_detached(nonce, header, &mut body)
        .expect("SRTP GCM encrypt");
    let mut out = Vec::with_capacity(header.len() + body.len() + tag.len());
    out.extend_from_slice(header);
    out.extend_from_slice(&body);
    out.extend_from_slice(tag.as_slice());
    out
}

fn unprotect_rtcp_gcm<C>(key: &C, salt: [u8; 12], packet: &[u8]) -> Option<Vec<u8>>
where
    C: AeadInPlace,
{
    if packet.len() < 8 + 4 + 16 {
        return None;
    }
    let idx_start = packet.len() - 4;
    let e_and_si = u32::from_be_bytes(packet[idx_start..].try_into().ok()?);
    let encrypted = (e_and_si & 0x8000_0000) != 0;
    let srtcp_index = e_and_si & 0x7fff_ffff;
    let ssrc = u32::from_be_bytes(packet[4..8].try_into().ok()?);
    let iv = rtcp_gcm_iv(salt, ssrc, srtcp_index);
    let nonce = Nonce::from_slice(&iv);
    let mut aad = [0u8; 12];
    aad[..8].copy_from_slice(&packet[..8]);
    aad[8..12].copy_from_slice(&e_and_si.to_be_bytes());
    if encrypted {
        let body = &packet[8..idx_start];
        if body.len() < 16 {
            return None;
        }
        let mut ciphertext = body[..body.len() - 16].to_vec();
        let tag = Tag::from_slice(&body[body.len() - 16..]);
        key.decrypt_in_place_detached(nonce, &aad, &mut ciphertext, tag)
            .ok()?;
        let mut out = packet[..8].to_vec();
        out.extend_from_slice(&ciphertext);
        Some(out)
    } else {
        None
    }
}

fn protect_rtcp_gcm<C>(
    key: &C,
    salt: [u8; 12],
    packet: &[u8],
    ssrc: u32,
    srtcp_index: u32,
) -> Vec<u8>
where
    C: AeadInPlace,
{
    let e_and_si = 0x8000_0000 | (srtcp_index & 0x7fff_ffff);
    let iv = rtcp_gcm_iv(salt, ssrc, srtcp_index);
    let nonce = Nonce::from_slice(&iv);
    let mut aad = [0u8; 12];
    aad[..8].copy_from_slice(&packet[..8]);
    aad[8..12].copy_from_slice(&e_and_si.to_be_bytes());
    let mut body = packet[8..].to_vec();
    let tag = key
        .encrypt_in_place_detached(nonce, &aad, &mut body)
        .expect("SRTCP GCM encrypt");
    let mut out = Vec::with_capacity(8 + body.len() + tag.len() + 4);
    out.extend_from_slice(&packet[..8]);
    out.extend_from_slice(&body);
    out.extend_from_slice(tag.as_slice());
    out.extend_from_slice(&e_and_si.to_be_bytes());
    out
}

fn rtp_gcm_iv(salt: [u8; 12], ssrc: u32, roc: u32, seq: u16) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[2..6].copy_from_slice(&ssrc.to_be_bytes());
    iv[6..10].copy_from_slice(&roc.to_be_bytes());
    iv[10..12].copy_from_slice(&seq.to_be_bytes());
    for i in 0..12 {
        iv[i] ^= salt[i];
    }
    iv
}

fn rtcp_gcm_iv(salt: [u8; 12], ssrc: u32, srtcp_index: u32) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[2..6].copy_from_slice(&ssrc.to_be_bytes());
    iv[8..12].copy_from_slice(&srtcp_index.to_be_bytes());
    for i in 0..12 {
        iv[i] ^= salt[i];
    }
    iv
}

fn parse_rtcp_feedback(packet: &[u8]) -> RtcpFeedback {
    let mut out = RtcpFeedback::default();
    let mut offset = 0usize;
    while offset + 4 <= packet.len() {
        let block = &packet[offset..];
        let words = u16::from_be_bytes([block[2], block[3]]) as usize + 1;
        let block_len = words * 4;
        if block_len == 0 || offset + block_len > packet.len() {
            break;
        }
        let fmt = block[0] & 0x1F;
        let packet_type = block[1];
        if matches!((packet_type, fmt), (206, 1) | (206, 4) | (192, 4)) {
            out.requests_keyframe = true;
        }
        if packet_type == 205 && fmt == 1 {
            parse_rtcp_generic_nack(&block[..block_len], &mut out.nack_sequences);
        }
        if packet_type == 201 {
            out.receiver_report = parse_rtcp_receiver_report(&block[..block_len]);
        }
        offset += block_len;
    }
    out
}

fn parse_rtcp_generic_nack(packet: &[u8], out: &mut Vec<u16>) {
    if packet.len() < 12 {
        return;
    }
    let mut offset = 12usize;
    while offset + 4 <= packet.len() {
        let pid = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let blp = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
        out.push(pid);
        for bit in 0..16u16 {
            if (blp & (1 << bit)) != 0 {
                out.push(pid.wrapping_add(bit + 1));
            }
        }
        offset += 4;
    }
}

fn parse_rtcp_receiver_report(packet: &[u8]) -> Option<ReceiverReportSummary> {
    if packet.len() < 8 {
        return None;
    }
    let report_count = (packet[0] & 0x1F) as usize;
    if report_count == 0 {
        return None;
    }
    let mut offset = 8usize;
    let mut max_fraction_lost = 0u8;
    let mut max_jitter = 0u32;
    let mut seen = 0usize;
    for _ in 0..report_count {
        if offset + 24 > packet.len() {
            break;
        }
        let fraction_lost = packet[offset + 4];
        let jitter = u32::from_be_bytes([
            packet[offset + 12],
            packet[offset + 13],
            packet[offset + 14],
            packet[offset + 15],
        ]);
        max_fraction_lost = max_fraction_lost.max(fraction_lost);
        max_jitter = max_jitter.max(jitter);
        seen += 1;
        offset += 24;
    }
    if seen == 0 {
        None
    } else {
        Some(ReceiverReportSummary {
            fraction_lost: max_fraction_lost,
            jitter: max_jitter,
        })
    }
}

fn derive_gcm_material_128(material: &KeyingMaterial, left: bool) -> Result<([u8; 16], [u8; 12])> {
    let master = slice_master::<16, 12>(material, left)?;
    let mut key = [0u8; 16];
    derive_aes_ctr_material::<Aes128, 16, 12>(&master.0, &master.1, 0, &mut key);
    let mut salt = [0u8; 12];
    derive_aes_ctr_material::<Aes128, 16, 12>(&master.0, &master.1, 2, &mut salt);
    Ok((key, salt))
}

fn derive_gcm_material_256(material: &KeyingMaterial, left: bool) -> Result<([u8; 32], [u8; 12])> {
    let master = slice_master::<32, 12>(material, left)?;
    let mut key = [0u8; 32];
    derive_aes_ctr_material::<Aes256, 32, 12>(&master.0, &master.1, 0, &mut key);
    let mut salt = [0u8; 12];
    derive_aes_ctr_material::<Aes256, 32, 12>(&master.0, &master.1, 2, &mut salt);
    Ok((key, salt))
}

fn derive_cm_material_128(
    material: &KeyingMaterial,
    left: bool,
) -> Result<([u8; 16], [u8; 20], [u8; 14])> {
    let master = slice_master::<16, 14>(material, left)?;
    let mut enc_key = [0u8; 16];
    derive_aes_ctr_material::<Aes128, 16, 14>(&master.0, &master.1, 0, &mut enc_key);
    let mut auth_key = [0u8; 20];
    derive_aes_ctr_material::<Aes128, 16, 14>(&master.0, &master.1, 1, &mut auth_key);
    let mut salt = [0u8; 14];
    derive_aes_ctr_material::<Aes128, 16, 14>(&master.0, &master.1, 2, &mut salt);
    Ok((enc_key, auth_key, salt))
}

fn slice_master<const ML: usize, const SL: usize>(
    material: &KeyingMaterial,
    left: bool,
) -> Result<([u8; ML], [u8; SL])> {
    if material.len() != ML * 2 + SL * 2 {
        bail!(
            "unexpected DTLS-SRTP keying material length {}",
            material.len()
        );
    }
    let (key_offset, salt_offset) = if left { (0, 0) } else { (ML, SL) };
    let mut master = [0u8; ML];
    let mut salt = [0u8; SL];
    master.copy_from_slice(&material[key_offset..key_offset + ML]);
    salt.copy_from_slice(&material[(ML * 2 + salt_offset)..(ML * 2 + salt_offset + SL)]);
    Ok((master, salt))
}

fn derive_aes_ctr_material<C, const ML: usize, const SL: usize>(
    master: &[u8; ML],
    salt: &[u8; SL],
    label: u8,
    out: &mut [u8],
) where
    C: BlockEncrypt + AesKeyInit,
{
    let cipher = C::new(GenericArray::from_slice(master));
    let mut input = [0u8; ML];
    input[..SL].copy_from_slice(salt);
    input[7] ^= label;
    let mut round = 0u16;
    let mut produced = 0usize;
    while produced < out.len() {
        input[ML - 2..].copy_from_slice(&round.to_be_bytes());
        let mut block = GenericArray::clone_from_slice(&input);
        cipher.encrypt_block(&mut block);
        for byte in block.iter() {
            if produced == out.len() {
                break;
            }
            out[produced] = *byte;
            produced += 1;
        }
        round = round.wrapping_add(1);
    }
}

fn protect_rtp_aes_cm_sha1_80(
    enc_key: &[u8; 16],
    auth_key: &[u8; 20],
    salt: [u8; 14],
    header: &[u8],
    sequence_number: u16,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.len() + payload.len() + 10);
    out.extend_from_slice(header);
    out.extend_from_slice(payload);
    let roc = 0u32;
    if !payload.is_empty() {
        let iv = rtp_aes_cm_iv(salt, ssrc, roc, sequence_number);
        aes_ctr_xor_in_place::<Aes128>(enc_key, &iv, &mut out[header.len()..]);
    }
    let mut auth_input = out.clone();
    auth_input.extend_from_slice(&roc.to_be_bytes());
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, auth_key),
        &auth_input,
    );
    out.extend_from_slice(&tag.as_ref()[..10]);
    out
}

fn protect_rtcp_aes_cm_sha1_80(auth_key: &[u8; 20], packet: &[u8], srtcp_index: u32) -> Vec<u8> {
    let e_and_si = srtcp_index & 0x7fff_ffff;
    let mut out = Vec::with_capacity(packet.len() + 4 + 10);
    out.extend_from_slice(packet);
    out.extend_from_slice(&e_and_si.to_be_bytes());
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, auth_key),
        &out,
    );
    out.extend_from_slice(&tag.as_ref()[..10]);
    out
}

fn unprotect_rtcp_aes_cm_sha1_80(
    enc_key: &[u8; 16],
    auth_key: &[u8; 20],
    salt: [u8; 14],
    packet: &[u8],
) -> Option<Vec<u8>> {
    if packet.len() < 8 + 4 + 10 {
        return None;
    }
    let auth_tag_start = packet.len() - 10;
    let index_start = auth_tag_start.checked_sub(4)?;
    let auth_input = &packet[..auth_tag_start];
    let auth_tag = &packet[auth_tag_start..];
    let expected_tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, auth_key),
        auth_input,
    );
    if !constant_time_eq(&expected_tag.as_ref()[..10], auth_tag) {
        return None;
    }

    let e_and_si = u32::from_be_bytes(packet[index_start..auth_tag_start].try_into().ok()?);
    let encrypted = (e_and_si & 0x8000_0000) != 0;
    let srtcp_index = e_and_si & 0x7fff_ffff;
    let mut out = packet[..index_start].to_vec();
    if encrypted {
        let ssrc = u32::from_be_bytes(out[4..8].try_into().ok()?);
        let iv = rtcp_aes_cm_iv(salt, ssrc, srtcp_index);
        aes_ctr_xor_in_place::<Aes128>(enc_key, &iv, &mut out[8..]);
    }
    Some(out)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn rtp_aes_cm_iv(salt: [u8; 14], ssrc: u32, roc: u32, seq: u16) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(&salt);
    iv[4..8]
        .iter_mut()
        .zip(ssrc.to_be_bytes())
        .for_each(|(dst, src)| *dst ^= src);
    let index = ((roc as u64) << 16) | seq as u64;
    let index_bytes = index.to_be_bytes();
    iv[8..14]
        .iter_mut()
        .zip(index_bytes[2..8].iter().copied())
        .for_each(|(dst, src)| *dst ^= src);
    iv
}

fn rtcp_aes_cm_iv(salt: [u8; 14], ssrc: u32, srtcp_index: u32) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(&salt);
    iv[4..8]
        .iter_mut()
        .zip(ssrc.to_be_bytes())
        .for_each(|(dst, src)| *dst ^= src);
    let index_bytes = (srtcp_index as u64).to_be_bytes();
    iv[8..14]
        .iter_mut()
        .zip(index_bytes[2..8].iter().copied())
        .for_each(|(dst, src)| *dst ^= src);
    iv
}

fn aes_ctr_xor_in_place<C>(key: &[u8; 16], iv: &[u8; 16], buf: &mut [u8])
where
    C: BlockEncrypt + AesKeyInit,
{
    let cipher = C::new(GenericArray::from_slice(key));
    let mut counter = *iv;
    let mut offset = 0usize;
    while offset < buf.len() {
        let mut block = GenericArray::clone_from_slice(&counter);
        cipher.encrypt_block(&mut block);
        let take = (buf.len() - offset).min(16);
        for i in 0..take {
            buf[offset + i] ^= block[i];
        }
        offset += take;
        let ctr = u16::from_be_bytes([counter[14], counter[15]]).wrapping_add(1);
        counter[14..16].copy_from_slice(&ctr.to_be_bytes());
    }
}

impl H264PacketizerState {
    fn packetize(&mut self, mtu: usize, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for nal in split_annexb_nalus(payload) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & H264_NALU_TYPE_MASK {
                H264_SPS_NALU_TYPE => {
                    self.sps = Some(nal.to_vec());
                    continue;
                }
                H264_PPS_NALU_TYPE => {
                    self.pps = Some(nal.to_vec());
                    continue;
                }
                H264_IDR_NALU_TYPE => {
                    if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
                        if let Some(stap_a) = make_h264_stap_a(mtu, sps, pps) {
                            out.push(stap_a);
                        }
                    }
                }
                _ => {}
            }
            emit_h264_nal(mtu, nal, &mut out);
        }
        if out.is_empty() && !payload.is_empty() {
            emit_h264_nal(mtu, payload, &mut out);
        }
        out
    }
}

fn make_h264_stap_a(mtu: usize, sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let size = 1 + 2 + sps.len() + 2 + pps.len();
    if size > mtu {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    out.push((sps[0] & H264_NALU_REF_IDC_MASK) | H264_STAPA_NALU_TYPE);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Some(out)
}

fn emit_h264_nal(mtu: usize, nal: &[u8], out: &mut Vec<Vec<u8>>) {
    if nal.is_empty() {
        return;
    }
    if nal.len() <= mtu {
        out.push(nal.to_vec());
        return;
    }
    if mtu <= 2 {
        return;
    }
    let nal_header = nal[0];
    let nal_type = nal_header & H264_NALU_TYPE_MASK;
    let nal_ref_idc = nal_header & H264_NALU_REF_IDC_MASK;
    let max_fragment = mtu - 2;
    let mut offset = 1usize;
    while offset < nal.len() {
        let remaining = nal.len() - offset;
        let take = remaining.min(max_fragment);
        let start = offset == 1;
        let end = offset + take == nal.len();
        let mut frag = Vec::with_capacity(2 + take);
        frag.push(H264_FUA_NALU_TYPE | nal_ref_idc);
        let mut fu_header = nal_type;
        if start {
            fu_header |= 0x80;
        }
        if end {
            fu_header |= 0x40;
        }
        frag.push(fu_header);
        frag.extend_from_slice(&nal[offset..offset + take]);
        out.push(frag);
        offset += take;
    }
}

fn split_annexb_nalus(payload: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(start) = find_annexb_start(payload, i) {
        let nal_start = start.0 + start.1;
        let next = find_annexb_start(payload, nal_start)
            .map(|v| v.0)
            .unwrap_or(payload.len());
        if nal_start < next {
            out.push(&payload[nal_start..next]);
        }
        i = next;
    }
    if out.is_empty() && !payload.is_empty() {
        out.push(payload);
    }
    out
}

fn find_annexb_start(payload: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 < payload.len() {
        if payload[i..].starts_with(&[0, 0, 1]) {
            return Some((i, 3));
        }
        if payload[i..].starts_with(&[0, 0, 0, 1]) {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn current_ntp_timestamp() -> (u32, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs().saturating_add(2_208_988_800);
    let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000u64;
    (secs as u32, frac as u32)
}

fn build_rtcp_sender_report(
    ssrc: u32,
    ntp_secs: u32,
    ntp_frac: u32,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    out.push(0x80);
    out.push(200);
    out.extend_from_slice(&6u16.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(&ntp_secs.to_be_bytes());
    out.extend_from_slice(&ntp_frac.to_be_bytes());
    out.extend_from_slice(&rtp_timestamp.to_be_bytes());
    out.extend_from_slice(&packet_count.to_be_bytes());
    out.extend_from_slice(&octet_count.to_be_bytes());
    out
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedOffer {
    has_video: bool,
    has_audio: bool,
    has_application: bool,
    mids: Vec<String>,
    candidate_count: usize,
    ice_ufrag: Option<String>,
    ice_pwd: Option<String>,
    fingerprint_sha256: Option<String>,
    setup_role: Option<String>,
    media: Vec<MediaSection>,
}

impl ParsedOffer {
    fn parse(sdp: &str) -> Result<Self> {
        let mut out = ParsedOffer::default();
        let mut current_media: Option<usize> = None;

        for raw in sdp.lines() {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix("m=") {
                if rest.starts_with("video ") {
                    out.has_video = true;
                    out.media.push(MediaSection::new(MediaKind::Video));
                    current_media = Some(out.media.len() - 1);
                } else if rest.starts_with("audio ") {
                    out.has_audio = true;
                    out.media.push(MediaSection::new(MediaKind::Audio));
                    current_media = Some(out.media.len() - 1);
                } else if rest.starts_with("application ") {
                    out.has_application = true;
                    out.media.push(MediaSection::new(MediaKind::Application));
                    current_media = Some(out.media.len() - 1);
                }
            } else if let Some(mid) = line.strip_prefix("a=mid:") {
                out.mids.push(mid.to_string());
                if let Some(ix) = current_media {
                    out.media[ix].mid = Some(mid.to_string());
                }
            } else if let Some(extmap) = line.strip_prefix("a=extmap:") {
                if let Some(ix) = current_media {
                    let mut parts = extmap.split_whitespace();
                    if let (Some(id_part), Some(uri)) = (parts.next(), parts.next()) {
                        let id_str = id_part.split('/').next().unwrap_or(id_part);
                        if uri == "urn:ietf:params:rtp-hdrext:sdes:mid" {
                            if let Ok(id) = id_str.parse::<u8>() {
                                out.media[ix].mid_ext_id = Some(id);
                            }
                        }
                    }
                }
            } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
                if let Some(ix) = current_media {
                    let mut parts = rtpmap.split_whitespace();
                    if let (Some(pt), Some(spec)) = (parts.next(), parts.next()) {
                        if let Ok(pt) = pt.parse::<u8>() {
                            let codec = spec.split('/').next().unwrap_or(spec);
                            out.media[ix].payloads.push(PayloadSpec {
                                pt,
                                codec: codec.to_ascii_uppercase(),
                                fmtp: None,
                            });
                        }
                    }
                }
            } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
                if let Some(ix) = current_media {
                    if let Some((pt_str, value)) = fmtp.split_once(' ') {
                        if let Ok(pt) = pt_str.parse::<u8>() {
                            if let Some(payload) =
                                out.media[ix].payloads.iter_mut().find(|p| p.pt == pt)
                            {
                                payload.fmtp = Some(value.trim().to_string());
                            }
                        }
                    }
                }
            } else if let Some(ufrag) = line.strip_prefix("a=ice-ufrag:") {
                out.ice_ufrag.get_or_insert_with(|| ufrag.to_string());
            } else if let Some(pwd) = line.strip_prefix("a=ice-pwd:") {
                out.ice_pwd.get_or_insert_with(|| pwd.to_string());
            } else if let Some(fingerprint) = line.strip_prefix("a=fingerprint:sha-256 ") {
                out.fingerprint_sha256
                    .get_or_insert_with(|| fingerprint.trim().to_ascii_uppercase());
            } else if let Some(setup) = line.strip_prefix("a=setup:") {
                out.setup_role.get_or_insert_with(|| setup.to_string());
            } else if line.starts_with("a=candidate:") {
                out.candidate_count += 1;
            }
        }

        if !out.has_video {
            bail!("phantom backend expects a video m-line in the SDP offer");
        }
        if !out.has_application {
            bail!("phantom backend expects an application m-line for DataChannel");
        }
        if out.ice_ufrag.is_none() || out.ice_pwd.is_none() {
            bail!("phantom backend expects ICE credentials in the SDP offer");
        }
        if out.fingerprint_sha256.is_none() {
            bail!("phantom backend expects a DTLS fingerprint in the SDP offer");
        }
        for media in &mut out.media {
            media.choose_codec();
        }
        let video = out
            .media
            .iter()
            .find(|m| m.kind == Some(MediaKind::Video))
            .and_then(|m| m.video_payload_type);
        if video.is_none() {
            bail!("phantom backend expects a packetization-mode=1 H264 payload in the SDP offer");
        }

        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Video,
    Audio,
    Application,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct MediaSection {
    kind: Option<MediaKind>,
    mid: Option<String>,
    mid_ext_id: Option<u8>,
    payloads: Vec<PayloadSpec>,
    video_payload_type: Option<u8>,
    video_fmtp: Option<String>,
    audio_payload_type: Option<u8>,
    audio_fmtp: Option<String>,
}

impl MediaSection {
    fn new(kind: MediaKind) -> Self {
        Self {
            kind: Some(kind),
            mid: None,
            mid_ext_id: None,
            payloads: Vec::new(),
            video_payload_type: None,
            video_fmtp: None,
            audio_payload_type: None,
            audio_fmtp: None,
        }
    }

    fn choose_codec(&mut self) {
        match self.kind {
            Some(MediaKind::Video) => {
                let preferred = self
                    .payloads
                    .iter()
                    .find(|p| {
                        p.codec == "H264"
                            && p.fmtp
                                .as_deref()
                                .unwrap_or_default()
                                .contains("packetization-mode=1")
                            && p.fmtp
                                .as_deref()
                                .unwrap_or_default()
                                .contains("profile-level-id=42e01f")
                    })
                    .or_else(|| {
                        self.payloads.iter().find(|p| {
                            p.codec == "H264"
                                && p.fmtp
                                    .as_deref()
                                    .unwrap_or_default()
                                    .contains("packetization-mode=1")
                        })
                    })
                    .or_else(|| self.payloads.iter().find(|p| p.codec == "H264"));
                if let Some(payload) = preferred {
                    self.video_payload_type = Some(payload.pt);
                    self.video_fmtp = payload.fmtp.clone();
                }
            }
            Some(MediaKind::Audio) => {
                if let Some(payload) = self.payloads.iter().find(|p| p.codec == "OPUS") {
                    self.audio_payload_type = Some(payload.pt);
                    self.audio_fmtp = payload.fmtp.clone();
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
struct PayloadSpec {
    pt: u8,
    codec: String,
    fmtp: Option<String>,
}

struct AnswerBuilder<'a> {
    offer: &'a ParsedOffer,
    params: &'a PhantomSessionParams,
}

impl<'a> AnswerBuilder<'a> {
    fn from_offer(offer: &'a ParsedOffer, params: &'a PhantomSessionParams) -> Self {
        Self { offer, params }
    }

    fn build(&self) -> String {
        let mut sdp = String::new();
        sdp.push_str("v=0\r\n");
        sdp.push_str("o=- 0 0 IN IP4 127.0.0.1\r\n");
        sdp.push_str("s=Phantom WebRTC\r\n");
        sdp.push_str("t=0 0\r\n");
        sdp.push_str("a=group:BUNDLE");
        for mid in &self.offer.mids {
            sdp.push(' ');
            sdp.push_str(mid);
        }
        sdp.push_str("\r\n");
        sdp.push_str("a=msid-semantic: WMS *\r\n");

        for media in &self.offer.media {
            match media.kind {
                Some(MediaKind::Video) => {
                    let pt = media.video_payload_type.unwrap_or(109);
                    sdp.push_str("m=video 9 UDP/TLS/RTP/SAVPF ");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str("\r\n");
                    sdp.push_str("c=IN IP4 0.0.0.0\r\n");
                    sdp.push_str("a=sendonly\r\n");
                    if let Some(mid) = &media.mid {
                        sdp.push_str("a=mid:");
                        sdp.push_str(mid);
                        sdp.push_str("\r\n");
                    }
                    if let Some(ext_id) = media.mid_ext_id {
                        sdp.push_str("a=extmap:");
                        sdp.push_str(&ext_id.to_string());
                        sdp.push_str(" urn:ietf:params:rtp-hdrext:sdes:mid\r\n");
                    }
                    sdp.push_str("a=rtcp-mux\r\n");
                    sdp.push_str("a=rtcp-rsize\r\n");
                    sdp.push_str("a=rtcp-fb:");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str(" ccm fir\r\n");
                    sdp.push_str("a=rtcp-fb:");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str(" nack\r\n");
                    sdp.push_str("a=rtcp-fb:");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str(" nack pli\r\n");
                    sdp.push_str("a=rtpmap:");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str(" H264/90000\r\n");
                    sdp.push_str("a=fmtp:");
                    sdp.push_str(&pt.to_string());
                    sdp.push(' ');
                    sdp.push_str(
                        media
                            .video_fmtp
                            .as_deref()
                            .unwrap_or(PHANTOM_H264_FMTP_FALLBACK),
                    );
                    sdp.push_str("\r\n");
                }
                Some(MediaKind::Audio) => {
                    let pt = media.audio_payload_type.unwrap_or(111);
                    sdp.push_str("m=audio 9 UDP/TLS/RTP/SAVPF ");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str("\r\n");
                    sdp.push_str("c=IN IP4 0.0.0.0\r\n");
                    sdp.push_str("a=sendonly\r\n");
                    if let Some(mid) = &media.mid {
                        sdp.push_str("a=mid:");
                        sdp.push_str(mid);
                        sdp.push_str("\r\n");
                    }
                    if let Some(ext_id) = media.mid_ext_id {
                        sdp.push_str("a=extmap:");
                        sdp.push_str(&ext_id.to_string());
                        sdp.push_str(" urn:ietf:params:rtp-hdrext:sdes:mid\r\n");
                    }
                    sdp.push_str("a=rtcp-mux\r\n");
                    sdp.push_str("a=rtcp-rsize\r\n");
                    sdp.push_str("a=rtpmap:");
                    sdp.push_str(&pt.to_string());
                    sdp.push_str(" opus/48000/2\r\n");
                    if let Some(fmtp) = &media.audio_fmtp {
                        sdp.push_str("a=fmtp:");
                        sdp.push_str(&pt.to_string());
                        sdp.push(' ');
                        sdp.push_str(fmtp);
                        sdp.push_str("\r\n");
                    }
                }
                Some(MediaKind::Application) => {
                    sdp.push_str("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n");
                    sdp.push_str("c=IN IP4 0.0.0.0\r\n");
                    if let Some(mid) = &media.mid {
                        sdp.push_str("a=mid:");
                        sdp.push_str(mid);
                        sdp.push_str("\r\n");
                    }
                    sdp.push_str("a=sctp-port:5000\r\n");
                    sdp.push_str("a=max-message-size:262144\r\n");
                }
                None => {}
            }
            sdp.push_str("a=ice-lite\r\n");
            sdp.push_str("a=ice-ufrag:");
            sdp.push_str(&self.params.ice_ufrag);
            sdp.push_str("\r\n");
            sdp.push_str("a=ice-pwd:");
            sdp.push_str(&self.params.ice_pwd);
            sdp.push_str("\r\n");
            sdp.push_str("a=setup:passive\r\n");
            sdp.push_str("a=fingerprint:sha-256 ");
            sdp.push_str(&self.params.fingerprint_sha256);
            sdp.push_str("\r\n");
            sdp.push_str("a=candidate:1 1 udp 2130706431 ");
            sdp.push_str(&self.params.candidate_addr.ip().to_string());
            sdp.push(' ');
            sdp.push_str(&self.params.candidate_addr.port().to_string());
            sdp.push_str(" typ host\r\n");
            sdp.push_str("a=end-of-candidates\r\n");
        }

        sdp
    }
}

#[derive(Debug, Clone)]
struct PhantomSessionParams {
    candidate_addr: SocketAddr,
    ice_ufrag: String,
    ice_pwd: String,
    fingerprint_sha256: String,
    remote_ice_ufrag: String,
    _remote_ice_pwd: String,
    remote_fingerprint_sha256: String,
    video_mid: Option<String>,
    audio_mid: Option<String>,
    video_payload_type: u8,
    audio_payload_type: u8,
    video_mid_ext_id: Option<u8>,
    audio_mid_ext_id: Option<u8>,
    dtls_config: Arc<DtlsConfig>,
    local_certificate: DtlsCertificate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StunBindingRequest {
    username: Option<String>,
}

fn parse_stun_binding_request(packet: &[u8]) -> Option<StunBindingRequest> {
    let msg = StunMessage::from_bytes(packet).ok()?;
    if !msg.has_class(MessageClass::Request) || msg.method() != BINDING {
        return None;
    }
    Some(StunBindingRequest {
        username: msg
            .attribute::<Username>()
            .ok()
            .map(|u| u.username().to_string()),
    })
}

fn build_stun_success_response(
    request_packet: &[u8],
    source: SocketAddr,
    params: &PhantomSessionParams,
) -> Option<Vec<u8>> {
    let request = StunMessage::from_bytes(request_packet).ok()?;
    if !request.has_class(MessageClass::Request) || request.method() != BINDING {
        return None;
    }
    let mut response = StunMessage::builder_success(&request, MessageWriteVec::new());
    response
        .add_attribute(&XorMappedAddress::new(source, request.transaction_id()))
        .ok()?;
    let credentials = ShortTermCredentials::new(params.ice_pwd.clone());
    response
        .add_message_integrity(&credentials.into(), IntegrityAlgorithm::Sha1)
        .ok()?;
    response.add_fingerprint().ok()?;
    Some(response.finish())
}

impl PhantomSessionParams {
    fn derive(candidate_addr: SocketAddr, offer: &ParsedOffer) -> Result<Self> {
        let local_certificate = make_dtls_certificate()?;
        let dtls_config = Arc::new(
            DtlsConfig::builder()
                .require_client_certificate(false)
                .use_server_cookie(false)
                .build()
                .context("build dimpl config")?,
        );

        Ok(Self {
            candidate_addr,
            ice_ufrag: format!("p{}", &Uuid::new_v4().simple().to_string()[..7]),
            ice_pwd: Uuid::new_v4().simple().to_string(),
            fingerprint_sha256: format_fingerprint(&calculate_fingerprint(
                &local_certificate.certificate,
            )),
            remote_ice_ufrag: offer.ice_ufrag.clone().unwrap_or_default(),
            _remote_ice_pwd: offer.ice_pwd.clone().unwrap_or_default(),
            remote_fingerprint_sha256: offer.fingerprint_sha256.clone().unwrap_or_default(),
            video_mid: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Video))
                .and_then(|m| m.mid.clone()),
            audio_mid: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Audio))
                .and_then(|m| m.mid.clone()),
            video_payload_type: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Video))
                .and_then(|m| m.video_payload_type)
                .unwrap_or(109),
            audio_payload_type: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Audio))
                .and_then(|m| m.audio_payload_type)
                .unwrap_or(111),
            video_mid_ext_id: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Video))
                .and_then(|m| m.mid_ext_id),
            audio_mid_ext_id: offer
                .media
                .iter()
                .find(|m| m.kind == Some(MediaKind::Audio))
                .and_then(|m| m.mid_ext_id),
            dtls_config,
            local_certificate,
        })
    }
}

fn make_dtls_certificate() -> Result<DtlsCertificate> {
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| anyhow!("generate DTLS key pair: {e}"))?;
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| anyhow!("build DTLS certificate params: {e}"))?;
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::OrganizationName, "Phantom".to_string());
    distinguished_name.push(DnType::CommonName, "Phantom DTLS Peer".to_string());
    params.distinguished_name = distinguished_name;
    params.is_ca = IsCa::NoCa;
    let not_before = rcgen::date_time_ymd(2026, 1, 1);
    let not_after = rcgen::date_time_ymd(2031, 1, 1);
    params.not_before = not_before;
    params.not_after = not_after;
    params.serial_number = Some(Uuid::new_v4().as_u128().to_be_bytes().to_vec().into());
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("self-sign DTLS certificate: {e}"))?;
    Ok(DtlsCertificate {
        certificate: cert.der().to_vec(),
        private_key: key_pair.serialize_der(),
    })
}

fn calculate_fingerprint(cert_der: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, cert_der).as_ref().to_vec()
}

fn format_fingerprint(fingerprint: &[u8]) -> String {
    fingerprint
        .iter()
        .map(|byte| format!("{:02X}", byte))
        .collect::<Vec<String>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::{
        build_stun_success_response, parse_stun_binding_request, protect_rtcp_aes_cm_sha1_80,
        unprotect_rtcp_aes_cm_sha1_80, AnswerBuilder, MediaTxState, ParsedOffer,
        PhantomSessionParams,
    };
    use crate::transport::webrtc::sctp::{parse_dcep_open_label, DCEP_OPEN};
    use stun_types::attribute::{Fingerprint, MessageIntegrity, Username, XorMappedAddress};
    use stun_types::message::{
        Message as StunMessage, MessageClass, MessageWrite, MessageWriteExt, MessageWriteVec,
        BINDING,
    };

    #[test]
    fn parses_minimal_browser_offer_shape() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 109\r\n",
            "a=mid:0\r\n",
            "a=rtpmap:109 H264/90000\r\n",
            "a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
            "a=mid:1\r\n",
            "a=rtpmap:111 opus/48000/2\r\n",
            "a=fmtp:111 minptime=10;useinbandfec=1\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=mid:2\r\n",
            "a=ice-ufrag:browserUfrag\r\n",
            "a=ice-pwd:browserPasswordValue\r\n",
            "a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
            "a=setup:actpass\r\n",
            "a=candidate:1 1 udp 2130706431 10.0.0.1 5000 typ host\r\n",
        );
        let parsed = ParsedOffer::parse(sdp).unwrap();
        assert!(parsed.has_video);
        assert!(parsed.has_audio);
        assert!(parsed.has_application);
        assert_eq!(parsed.candidate_count, 1);
        assert_eq!(parsed.mids, vec!["0", "1", "2"]);
        assert_eq!(parsed.ice_ufrag.as_deref(), Some("browserUfrag"));
        assert_eq!(parsed.ice_pwd.as_deref(), Some("browserPasswordValue"));
        assert_eq!(parsed.setup_role.as_deref(), Some("actpass"));
        assert_eq!(parsed.media.len(), 3);
        assert_eq!(parsed.media[0].video_payload_type, Some(109));
        assert_eq!(parsed.media[1].audio_payload_type, Some(111));
    }

    #[test]
    fn rejects_offer_without_video() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        let err = ParsedOffer::parse(sdp).unwrap_err().to_string();
        assert!(err.contains("video m-line"));
    }

    #[test]
    fn builds_answer_with_matching_mids() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 109\r\n",
            "a=mid:0\r\n",
            "a=rtpmap:109 H264/90000\r\n",
            "a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=mid:data\r\n",
            "a=ice-ufrag:browserUfrag\r\n",
            "a=ice-pwd:browserPasswordValue\r\n",
            "a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
        );
        let offer = ParsedOffer::parse(sdp).unwrap();
        let params =
            PhantomSessionParams::derive("10.0.0.5:9903".parse().unwrap(), &offer).unwrap();
        let answer = AnswerBuilder::from_offer(&offer, &params).build();
        assert!(answer.contains("a=group:BUNDLE 0 data"));
        assert!(answer.contains("m=video 9 UDP/TLS/RTP/SAVPF 109"));
        assert!(answer.contains("a=mid:0"));
        assert!(answer.contains("a=mid:data"));
        assert!(answer.contains("a=candidate:1 1 udp 2130706431 10.0.0.5 9903 typ host"));
        assert!(answer.contains("a=ice-ufrag:"));
        assert!(answer.contains("a=ice-pwd:"));
        assert!(answer.contains("a=rtpmap:109 H264/90000"));
        assert!(answer.contains(
            "a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n"
        ));
        assert!(answer.contains(&format!(
            "a=fingerprint:sha-256 {}\r\n",
            params.fingerprint_sha256
        )));
        assert_eq!(params.remote_ice_ufrag, "browserUfrag");
        assert_eq!(params._remote_ice_pwd, "browserPasswordValue");
        assert_eq!(params.video_payload_type, 109);
        assert_eq!(
            params.remote_fingerprint_sha256,
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn rejects_offer_without_ice_credentials() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 109\r\n",
            "a=mid:0\r\n",
            "a=rtpmap:109 H264/90000\r\n",
            "a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=mid:data\r\n",
        );
        let err = ParsedOffer::parse(sdp).unwrap_err().to_string();
        assert!(err.contains("ICE credentials"));
    }

    #[test]
    fn builds_stun_success_response_with_expected_shape() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 109\r\n",
            "a=mid:0\r\n",
            "a=rtpmap:109 H264/90000\r\n",
            "a=fmtp:109 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
            "a=mid:data\r\n",
            "a=ice-ufrag:browserUfrag\r\n",
            "a=ice-pwd:browserPasswordValue\r\n",
            "a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n",
        );
        let offer = ParsedOffer::parse(sdp).unwrap();
        let params =
            PhantomSessionParams::derive("10.0.0.5:9903".parse().unwrap(), &offer).unwrap();
        let username = format!("{}:{}", params.ice_ufrag, params.remote_ice_ufrag);
        let mut request = StunMessage::builder_request(BINDING, MessageWriteVec::new());
        request
            .add_attribute(&Username::new(&username).unwrap())
            .unwrap();
        let request = request.finish();
        let parsed = parse_stun_binding_request(&request).unwrap();
        assert_eq!(parsed.username.as_deref(), Some(username.as_str()));

        let response =
            build_stun_success_response(&request, "10.0.0.9:50000".parse().unwrap(), &params)
                .unwrap();
        let response = StunMessage::from_bytes(&response).unwrap();
        assert!(response.has_class(MessageClass::Success));
        assert_eq!(response.method(), BINDING);
        assert!(response.attribute::<MessageIntegrity>().is_ok());
        assert!(response.attribute::<Fingerprint>().is_ok());
        let mapped = response.attribute::<XorMappedAddress>().unwrap();
        assert_eq!(
            mapped.addr(response.transaction_id()),
            "10.0.0.9:50000".parse().unwrap()
        );
    }

    #[test]
    fn parses_dcep_open_label() {
        let mut payload = vec![0u8; 12];
        payload[0] = DCEP_OPEN;
        payload[8..10].copy_from_slice(&(5u16).to_be_bytes());
        payload[10..12].copy_from_slice(&(0u16).to_be_bytes());
        payload.extend_from_slice(b"input");
        assert_eq!(parse_dcep_open_label(&payload).as_deref(), Some("input"));
    }

    #[test]
    fn aes_cm_srtcp_unprotect_accepts_authentic_plain_rtcp() {
        let enc_key = [0x11; 16];
        let auth_key = [0x22; 20];
        let salt = [0x33; 14];
        let rtcp = vec![
            0x81, 205, 0x00, 0x03, // RTPFB Generic NACK, 4 words
            0x12, 0x34, 0x56, 0x78, // sender SSRC
            0x87, 0x65, 0x43, 0x21, // media SSRC
            0x00, 0x09, 0x00, 0x00, // one lost sequence
        ];

        let protected = protect_rtcp_aes_cm_sha1_80(&auth_key, &rtcp, 7);
        let unprotected =
            unprotect_rtcp_aes_cm_sha1_80(&enc_key, &auth_key, salt, &protected).unwrap();

        assert_eq!(unprotected, rtcp);
    }

    #[test]
    fn aes_cm_srtcp_unprotect_rejects_bad_auth_tag() {
        let enc_key = [0x11; 16];
        let auth_key = [0x22; 20];
        let salt = [0x33; 14];
        let rtcp = vec![
            0x81, 205, 0x00, 0x03, 0x12, 0x34, 0x56, 0x78, 0x87, 0x65, 0x43, 0x21, 0x00, 0x09,
            0x00, 0x00,
        ];
        let mut protected = protect_rtcp_aes_cm_sha1_80(&auth_key, &rtcp, 7);
        let last = protected.last_mut().unwrap();
        *last ^= 0x01;

        assert!(unprotect_rtcp_aes_cm_sha1_80(&enc_key, &auth_key, salt, &protected).is_none());
    }

    #[test]
    fn video_timestamp_now_never_moves_backwards() {
        let mut tx = MediaTxState::new();
        tx.video_last_rtp_timestamp = tx.video_timestamp.wrapping_add(100);

        let first = tx.video_timestamp_now();
        assert_eq!(first, tx.video_last_rtp_timestamp.wrapping_add(1));

        tx.video_last_rtp_timestamp = first;
        let second = tx.video_timestamp_now();
        assert_eq!(second, first.wrapping_add(1));
    }
}
