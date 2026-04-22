use super::{
    BackendClient, MediaAudioFrame, RtcMode, WebRtcReceiver, WebRtcSender, make_session_bridge,
    sctp::{DataPpi, PhantomSctpStack, SctpNotice},
};
use aes::cipher::{BlockEncrypt, KeyInit as AesKeyInit, generic_array::GenericArray};
use aes::{Aes128, Aes256};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce, Tag};
use anyhow::{Context, Result, anyhow, bail};
use dimpl::{Config as DtlsConfig, Dtls, DtlsCertificate, KeyingMaterial, Output as DtlsOutput, SrtpProfile};
use phantom_core::encode::EncodedFrame;
use phantom_core::protocol::Message;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;
use ring::hmac;
use ring::digest;
use rcgen::{CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, PKCS_ECDSA_P256_SHA256};

const DCEP_OPEN: u8 = 0x03;
const DCEP_ACK: u8 = 0x02;
const RTP_HEADER_LEN: usize = 12;
const RTP_MTU: usize = 1200;
const H264_FUA_NALU_TYPE: u8 = 28;
const H264_STAPA_NALU_TYPE: u8 = 24;
const H264_NALU_TYPE_MASK: u8 = 0x1F;
const H264_NALU_REF_IDC_MASK: u8 = 0x60;
const H264_SPS_NALU_TYPE: u8 = 7;
const H264_PPS_NALU_TYPE: u8 = 8;
const H264_IDR_NALU_TYPE: u8 = 5;
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_SUCCESS: u16 = 0x0101;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_ATTR_USERNAME: u16 = 0x0006;
const STUN_ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const STUN_ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const STUN_ATTR_FINGERPRINT: u16 = 0x8028;
const STUN_FINGERPRINT_XOR: u32 = 0x5354_554e;

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
    pending_transmits: Vec<(SocketAddr, Vec<u8>)>,
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
            pending_transmits: Vec::new(),
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

    fn handle_dcep_open(
        &mut self,
        stream_id: u16,
        payload: &[u8],
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        let Some(label) = parse_dcep_open_label(payload) else {
            return;
        };
        tracing::info!(stream_id, label = %label, "phantom backend DataChannel opened");
        match label.as_str() {
            "input" => self.input_stream = Some(stream_id),
            "control" => self.control_stream = Some(stream_id),
            _ => {}
        }
        self.sctp.write_stream(stream_id, &[DCEP_ACK], DataPpi::Dcep);
        self.maybe_publish_session(session_slot, notify_tx);
    }

    fn handle_stream_message(
        &mut self,
        stream_id: u16,
        ppi: DataPpi,
        payload: &[u8],
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        match ppi {
            DataPpi::Dcep if payload.first().copied() == Some(DCEP_OPEN) => {
                self.handle_dcep_open(stream_id, payload, session_slot, notify_tx);
            }
            DataPpi::Dcep => {}
            DataPpi::Binary | DataPpi::BinaryEmpty | DataPpi::String | DataPpi::StringEmpty => {
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
            _ => {}
        }
    }

    fn handle_readable_stream(
        &mut self,
        stream_id: u16,
        session_slot: &Arc<Mutex<Option<(WebRtcSender, WebRtcReceiver)>>>,
        notify_tx: &mpsc::Sender<()>,
    ) {
        for (ppi, payload) in self.sctp.read_stream_messages(stream_id) {
            self.handle_stream_message(stream_id, ppi, &payload, session_slot, notify_tx);
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
                SctpNotice::StreamOpened(_) => {
                    for id in self.sctp.accept_streams() {
                        tracing::debug!(id, "phantom backend accepted SCTP stream");
                    }
                }
                SctpNotice::StreamReadable(id) => {
                    self.handle_readable_stream(id, session_slot, notify_tx);
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
        self.sctp
            .drain_transmits(now, |chunk| {
                let _ = self.dtls.send_application_data(chunk);
            });
    }

    fn send_video_frame(&mut self, frame: &EncodedFrame) {
        let Some(source) = self.last_source else {
            return;
        };
        let Some(srtp) = self.srtp_tx.as_mut() else {
            return;
        };
        let mut payloads = self
            .media_tx
            .h264
            .packetize(RTP_MTU.saturating_sub(RTP_HEADER_LEN + 16), &frame.data);
        let packet_count = payloads.len();
        for (i, payload) in payloads.drain(..).enumerate() {
            let marker = i + 1 == packet_count;
            let packet = srtp.protect_rtp(
                self.params.video_payload_type,
                marker,
                self.media_tx.video_seq,
                self.media_tx.video_timestamp,
                self.media_tx.video_ssrc,
                self.params.video_mid_ext_id,
                self.params.video_mid.as_deref(),
                &payload,
            );
            self.pending_transmits.push((source, packet));
            self.media_tx.video_seq = self.media_tx.video_seq.wrapping_add(1);
        }
        self.media_tx.video_timestamp = self.media_tx.video_timestamp.wrapping_add(3_000);
    }

    fn send_audio_frame(&mut self, frame: &MediaAudioFrame) {
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
        self.pending_transmits.push((source, packet));
        self.media_tx.audio_seq = self.media_tx.audio_seq.wrapping_add(1);
        self.media_tx.audio_timestamp = self
            .media_tx
            .audio_timestamp
            .wrapping_add((frame.sample_rate / 50).max(1));
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
        for (addr, packet) in self.pending_transmits.drain(..) {
            if let Err(error) = socket.send_to(&packet, addr) {
                tracing::warn!(%addr, len = packet.len(), %error, "phantom backend failed to send UDP packet");
            }
        }
        loop {
            match self.dtls.poll_output(&mut self.out_buf) {
                DtlsOutput::Packet(packet) => {
                    if let Some(addr) = self.last_source {
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
            video_frames.extend(std::iter::from_fn(|| rx.try_recv().ok()));
        }
        if let Some(rx) = &self.media_audio_rx {
            audio_frames.extend(std::iter::from_fn(|| rx.try_recv().ok()));
        }
        let Some(stream_id) = self.control_stream else {
            return;
        };
        for msg in control_msgs {
            self.sctp.write_stream(stream_id, &msg, DataPpi::Binary);
        }
        for frame in &video_frames {
            self.send_video_frame(frame);
        }
        for frame in &audio_frames {
            self.send_audio_frame(frame);
        }
    }

    fn handle_receive(
        &mut self,
        _candidate_addr: SocketAddr,
        source: SocketAddr,
        contents: &[u8],
    ) {
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
                if let Some(response) = build_stun_success_response(contents, source, &self.params) {
                    self.pending_transmits.push((source, response));
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
                    if rtcp_requests_keyframe(&rtcp) {
                        tracing::debug!("phantom backend received RTCP PLI/FIR");
                        if let Some(tx) = &self.control_in_tx {
                            let _ = tx.send(bincode::serialize(&Message::RequestKeyframe).unwrap_or_default());
                        }
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

fn parse_dcep_open_label(payload: &[u8]) -> Option<String> {
    if payload.len() < 12 || payload[0] != DCEP_OPEN {
        return None;
    }
    let label_len = u16::from_be_bytes([payload[8], payload[9]]) as usize;
    let protocol_len = u16::from_be_bytes([payload[10], payload[11]]) as usize;
    let label_start = 12usize;
    let label_end = label_start.checked_add(label_len)?;
    let protocol_end = label_end.checked_add(protocol_len)?;
    if protocol_end > payload.len() {
        return None;
    }
    std::str::from_utf8(&payload[label_start..label_end])
        .ok()
        .map(|label| label.to_string())
}

fn is_rtp_packet(packet: &[u8]) -> bool {
    packet.len() >= 12
        && packet.first().map(|b| (0x80..=0xBF).contains(b)).unwrap_or(false)
        && packet.get(1).map(|b| *b < 192 || *b > 223).unwrap_or(false)
}

fn is_rtcp_packet(packet: &[u8]) -> bool {
    packet.len() >= 8
        && packet.first().map(|b| (0x80..=0xBF).contains(b)).unwrap_or(false)
        && packet.get(1).map(|b| (192..=223).contains(b)).unwrap_or(false)
}

#[derive(Debug, Clone)]
struct MediaTxState {
    video_ssrc: u32,
    audio_ssrc: u32,
    video_seq: u16,
    audio_seq: u16,
    video_timestamp: u32,
    audio_timestamp: u32,
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
            audio_timestamp: (seed >> 64) as u32,
            h264: H264PacketizerState::default(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct H264PacketizerState {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

struct PhantomSrtpTxContext {
    rtp: PhantomSrtpCipher,
}

struct PhantomSrtpRxContext {
    rtcp: PhantomSrtpCipher,
}

enum PhantomSrtpCipher {
    AeadAes128Gcm { key: Aes128Gcm, salt: [u8; 12] },
    AeadAes256Gcm { key: Aes256Gcm, salt: [u8; 12] },
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
                    key: Aes128Gcm::new_from_slice(&key).map_err(|_| anyhow!("invalid AES-128-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AEAD_AES_256_GCM => {
                let (key, salt) = derive_gcm_material_256(material, left)?;
                PhantomSrtpCipher::AeadAes256Gcm {
                    key: Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("invalid AES-256-GCM key"))?,
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
            PhantomSrtpCipher::AeadAes128Gcm { key, salt } => {
                protect_rtp_gcm(key, *salt, &header, sequence_number, timestamp, ssrc, payload)
            }
            PhantomSrtpCipher::AeadAes256Gcm { key, salt } => {
                protect_rtp_gcm(key, *salt, &header, sequence_number, timestamp, ssrc, payload)
            }
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
}

impl PhantomSrtpRxContext {
    fn new(profile: SrtpProfile, material: &KeyingMaterial, active: bool) -> Result<Self> {
        let left = active;
        let rtcp = match profile {
            SrtpProfile::AEAD_AES_128_GCM => {
                let (key, salt) = derive_gcm_material_128(material, left)?;
                PhantomSrtpCipher::AeadAes128Gcm {
                    key: Aes128Gcm::new_from_slice(&key).map_err(|_| anyhow!("invalid AES-128-GCM key"))?,
                    salt,
                }
            }
            SrtpProfile::AEAD_AES_256_GCM => {
                let (key, salt) = derive_gcm_material_256(material, left)?;
                PhantomSrtpCipher::AeadAes256Gcm {
                    key: Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("invalid AES-256-GCM key"))?,
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
            PhantomSrtpCipher::AeadAes128Gcm { key, salt } => unprotect_rtcp_gcm(key, *salt, packet),
            PhantomSrtpCipher::AeadAes256Gcm { key, salt } => unprotect_rtcp_gcm(key, *salt, packet),
            PhantomSrtpCipher::Aes128CmSha1_80 { .. } => None,
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
        key.decrypt_in_place_detached(nonce, &aad, &mut ciphertext, tag).ok()?;
        let mut out = packet[..8].to_vec();
        out.extend_from_slice(&ciphertext);
        Some(out)
    } else {
        None
    }
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

fn rtcp_requests_keyframe(packet: &[u8]) -> bool {
    if packet.len() < 12 {
        return false;
    }
    let fmt = packet[0] & 0x1F;
    let packet_type = packet[1];
    matches!((packet_type, fmt), (206, 1) | (206, 4) | (192, 4))
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

fn derive_cm_material_128(material: &KeyingMaterial, left: bool) -> Result<([u8; 16], [u8; 20], [u8; 14])> {
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
        bail!("unexpected DTLS-SRTP keying material length {}", material.len());
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
        let next = find_annexb_start(payload, nal_start).map(|v| v.0).unwrap_or(payload.len());
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
                            if let Some(payload) = out.media[ix].payloads.iter_mut().find(|p| p.pt == pt) {
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
                            && p.fmtp.as_deref().unwrap_or_default().contains("packetization-mode=1")
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
                            .unwrap_or("packetization-mode=1;profile-level-id=42e01f;level-asymmetry-allowed=1"),
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
    transaction_id: [u8; 12],
}

fn parse_stun_binding_request(packet: &[u8]) -> Option<StunBindingRequest> {
    let header = parse_stun_header(packet)?;
    if header.msg_type != STUN_BINDING_REQUEST {
        return None;
    }
    let mut username = None;
    for attribute in parse_stun_attributes(packet, header.body_len)? {
        if attribute.ty == STUN_ATTR_USERNAME {
            username = std::str::from_utf8(attribute.value).ok().map(|s| s.to_string());
        }
    }
    Some(StunBindingRequest {
        username,
        transaction_id: header.transaction_id,
    })
}

fn build_stun_success_response(
    request_packet: &[u8],
    source: SocketAddr,
    params: &PhantomSessionParams,
) -> Option<Vec<u8>> {
    let request = parse_stun_binding_request(request_packet)?;
    let mut response = Vec::with_capacity(64);
    response.extend_from_slice(&STUN_BINDING_SUCCESS.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    response.extend_from_slice(&request.transaction_id);
    append_stun_xor_mapped_address(&mut response, source, request.transaction_id);
    append_stun_message_integrity(&mut response, params.ice_pwd.as_bytes());
    append_stun_fingerprint(&mut response);
    let body_len = response.len().checked_sub(20)? as u16;
    response[2..4].copy_from_slice(&body_len.to_be_bytes());
    Some(response)
}

#[derive(Debug, Clone, Copy)]
struct StunHeader {
    msg_type: u16,
    body_len: usize,
    transaction_id: [u8; 12],
}

#[derive(Debug, Clone, Copy)]
struct StunAttribute<'a> {
    ty: u16,
    value: &'a [u8],
}

fn parse_stun_header(packet: &[u8]) -> Option<StunHeader> {
    if packet.len() < 20 {
        return None;
    }
    if packet[0] & 0b1100_0000 != 0 {
        return None;
    }
    let msg_type = u16::from_be_bytes(packet[0..2].try_into().ok()?);
    let body_len = u16::from_be_bytes(packet[2..4].try_into().ok()?) as usize;
    if packet.len() != 20 + body_len {
        return None;
    }
    let magic_cookie = u32::from_be_bytes(packet[4..8].try_into().ok()?);
    if magic_cookie != STUN_MAGIC_COOKIE {
        return None;
    }
    let mut transaction_id = [0u8; 12];
    transaction_id.copy_from_slice(&packet[8..20]);
    Some(StunHeader {
        msg_type,
        body_len,
        transaction_id,
    })
}

fn parse_stun_attributes(packet: &[u8], body_len: usize) -> Option<Vec<StunAttribute<'_>>> {
    let mut out = Vec::new();
    let mut offset = 20usize;
    let end = 20usize.checked_add(body_len)?;
    while offset < end {
        let header_end = offset.checked_add(4)?;
        if header_end > end {
            return None;
        }
        let ty = u16::from_be_bytes(packet[offset..offset + 2].try_into().ok()?);
        let len = u16::from_be_bytes(packet[offset + 2..offset + 4].try_into().ok()?) as usize;
        let value_start = header_end;
        let value_end = value_start.checked_add(len)?;
        if value_end > end {
            return None;
        }
        out.push(StunAttribute {
            ty,
            value: &packet[value_start..value_end],
        });
        offset = align4(value_end);
    }
    if offset != end {
        return None;
    }
    Some(out)
}

fn append_stun_attribute(buf: &mut Vec<u8>, ty: u16, value: &[u8]) {
    buf.extend_from_slice(&ty.to_be_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn append_stun_xor_mapped_address(buf: &mut Vec<u8>, source: SocketAddr, transaction_id: [u8; 12]) {
    let mut value = Vec::with_capacity(match source {
        SocketAddr::V4(_) => 8,
        SocketAddr::V6(_) => 20,
    });
    value.push(0);
    match source {
        SocketAddr::V4(addr) => {
            value.push(0x01);
            let xport = addr.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
            value.extend_from_slice(&xport.to_be_bytes());
            let mut ip = addr.ip().octets();
            let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
            for (dst, mask) in ip.iter_mut().zip(cookie) {
                *dst ^= mask;
            }
            value.extend_from_slice(&ip);
        }
        SocketAddr::V6(addr) => {
            value.push(0x02);
            let xport = addr.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
            value.extend_from_slice(&xport.to_be_bytes());
            let mut ip = addr.ip().octets();
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(&transaction_id);
            for (dst, key) in ip.iter_mut().zip(mask) {
                *dst ^= key;
            }
            value.extend_from_slice(&ip);
        }
    }
    append_stun_attribute(buf, STUN_ATTR_XOR_MAPPED_ADDRESS, &value);
}

fn append_stun_message_integrity(buf: &mut Vec<u8>, key: &[u8]) {
    let attr_start = buf.len();
    buf.extend_from_slice(&STUN_ATTR_MESSAGE_INTEGRITY.to_be_bytes());
    buf.extend_from_slice(&20u16.to_be_bytes());
    let digest_start = buf.len();
    buf.resize(digest_start + 20, 0);
    let body_len = (buf.len() - 20) as u16;
    buf[2..4].copy_from_slice(&body_len.to_be_bytes());
    let digest = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key),
        &buf[..digest_start + 20],
    );
    buf[digest_start..digest_start + 20].copy_from_slice(&digest.as_ref()[..20]);
    debug_assert_eq!(attr_start + 24, buf.len());
}

fn append_stun_fingerprint(buf: &mut Vec<u8>) {
    let mut crc_input = buf.clone();
    let final_body_len = (buf.len() - 20 + 8) as u16;
    crc_input[2..4].copy_from_slice(&final_body_len.to_be_bytes());
    let fingerprint = crc32fast::hash(&crc_input) ^ STUN_FINGERPRINT_XOR;
    append_stun_attribute(buf, STUN_ATTR_FINGERPRINT, &fingerprint.to_be_bytes());
    let body_len = (buf.len() - 20) as u16;
    buf[2..4].copy_from_slice(&body_len.to_be_bytes());
}

#[cfg(test)]
fn decode_stun_xor_mapped_address(value: &[u8], transaction_id: [u8; 12]) -> Option<SocketAddr> {
    if value.len() < 4 || value[0] != 0 {
        return None;
    }
    let port = u16::from_be_bytes(value[2..4].try_into().ok()?) ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
    match value[1] {
        0x01 if value.len() == 8 => {
            let mut ip = [0u8; 4];
            ip.copy_from_slice(&value[4..8]);
            let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
            for (dst, mask) in ip.iter_mut().zip(cookie) {
                *dst ^= mask;
            }
            Some(SocketAddr::from((ip, port)))
        }
        0x02 if value.len() == 20 => {
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&value[4..20]);
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(&transaction_id);
            for (dst, key) in ip.iter_mut().zip(mask) {
                *dst ^= key;
            }
            Some(SocketAddr::from((ip, port)))
        }
        _ => None,
    }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
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
            fingerprint_sha256: format_fingerprint(&calculate_fingerprint(&local_certificate.certificate)),
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
        AnswerBuilder, DCEP_OPEN, ParsedOffer, PhantomSessionParams, STUN_ATTR_FINGERPRINT,
        STUN_ATTR_MESSAGE_INTEGRITY, STUN_ATTR_USERNAME, STUN_ATTR_XOR_MAPPED_ADDRESS,
        STUN_BINDING_SUCCESS, STUN_MAGIC_COOKIE, build_stun_success_response,
        decode_stun_xor_mapped_address, parse_dcep_open_label, parse_stun_attributes,
        parse_stun_binding_request, parse_stun_header,
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
        let params = PhantomSessionParams::derive("10.0.0.5:9903".parse().unwrap(), &offer).unwrap();
        let answer = AnswerBuilder::from_offer(&offer, &params).build();
        assert!(answer.contains("a=group:BUNDLE 0 data"));
        assert!(answer.contains("m=video 9 UDP/TLS/RTP/SAVPF 109"));
        assert!(answer.contains("a=mid:0"));
        assert!(answer.contains("a=mid:data"));
        assert!(answer.contains("a=candidate:1 1 udp 2130706431 10.0.0.5 9903 typ host"));
        assert!(answer.contains("a=ice-ufrag:"));
        assert!(answer.contains("a=ice-pwd:"));
        assert!(answer.contains("a=rtpmap:109 H264/90000"));
        assert!(answer.contains(&format!("a=fingerprint:sha-256 {}\r\n", params.fingerprint_sha256)));
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
        let params = PhantomSessionParams::derive("10.0.0.5:9903".parse().unwrap(), &offer).unwrap();
        let username = format!("{}:{}", params.ice_ufrag, params.remote_ice_ufrag);
        let mut request = Vec::new();
        request.extend_from_slice(&0x0001u16.to_be_bytes());
        request.extend_from_slice(&8u16.to_be_bytes());
        request.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        request.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        request.extend_from_slice(&STUN_ATTR_USERNAME.to_be_bytes());
        request.extend_from_slice(&(username.len() as u16).to_be_bytes());
        request.extend_from_slice(username.as_bytes());
        while request.len() % 4 != 0 {
            request.push(0);
        }
        let request_len = (request.len() - 20) as u16;
        request[2..4].copy_from_slice(&request_len.to_be_bytes());
        let parsed = parse_stun_binding_request(&request).unwrap();
        assert_eq!(parsed.username.as_deref(), Some(username.as_str()));

        let response = build_stun_success_response(&request, "10.0.0.9:50000".parse().unwrap(), &params).unwrap();
        let header = parse_stun_header(&response).unwrap();
        assert_eq!(header.msg_type, STUN_BINDING_SUCCESS);
        let attrs = parse_stun_attributes(&response, header.body_len).unwrap();
        assert!(attrs.iter().any(|a| a.ty == STUN_ATTR_MESSAGE_INTEGRITY && a.value.len() == 20));
        assert!(attrs.iter().any(|a| a.ty == STUN_ATTR_FINGERPRINT && a.value.len() == 4));
        let mapped = attrs
            .iter()
            .find(|a| a.ty == STUN_ATTR_XOR_MAPPED_ADDRESS)
            .and_then(|a| decode_stun_xor_mapped_address(a.value, header.transaction_id))
            .unwrap();
        assert_eq!(mapped, "10.0.0.9:50000".parse().unwrap());
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
}
