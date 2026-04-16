//! Phantom remote desktop client for the browser (WebAssembly).
//!
//! Runs in a web page, connecting to the server via WebSocket or WebRTC
//! DataChannel. Decodes H.264 video using the browser's WebCodecs API
//! and renders to an HTML5 canvas. Sends keyboard/mouse input back to
//! the server and supports clipboard paste.

use phantom_core::encode::{TileEncoding, VideoCodec};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
use phantom_core::protocol::Message;
use phantom_core::tile::TILE_SIZE;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    console, CanvasRenderingContext2d, HtmlCanvasElement, KeyboardEvent, MessageEvent, MouseEvent,
    WebSocket, WheelEvent,
};

// -- WebCodecs bindings (not in web-sys yet) --

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = VideoDecoder)]
    type JsVideoDecoder;
    #[wasm_bindgen(constructor, js_class = "VideoDecoder")]
    fn new(init: &JsValue) -> JsVideoDecoder;
    #[wasm_bindgen(method, js_class = "VideoDecoder")]
    fn configure(this: &JsVideoDecoder, config: &JsValue);
    #[wasm_bindgen(method, js_class = "VideoDecoder")]
    fn decode(this: &JsVideoDecoder, chunk: &JsValue);

    #[wasm_bindgen(js_name = EncodedVideoChunk)]
    type JsEncodedVideoChunk;
    #[wasm_bindgen(constructor, js_class = "EncodedVideoChunk")]
    fn new(init: &JsValue) -> JsEncodedVideoChunk;

    // -- WebCodecs AudioDecoder bindings --
    #[wasm_bindgen(js_name = AudioDecoder)]
    type JsAudioDecoder;
    #[wasm_bindgen(constructor, js_class = "AudioDecoder")]
    fn new(init: &JsValue) -> JsAudioDecoder;
    #[wasm_bindgen(method, js_class = "AudioDecoder")]
    fn configure(this: &JsAudioDecoder, config: &JsValue);
    #[wasm_bindgen(method, js_class = "AudioDecoder")]
    fn decode(this: &JsAudioDecoder, chunk: &JsValue);

    #[wasm_bindgen(js_name = EncodedAudioChunk)]
    type JsEncodedAudioChunk;
    #[wasm_bindgen(constructor, js_class = "EncodedAudioChunk")]
    fn new(init: &JsValue) -> JsEncodedAudioChunk;

    // AudioData from WebCodecs output callback
    #[wasm_bindgen(js_name = AudioData)]
    type JsAudioData;
    #[wasm_bindgen(method, getter, js_class = "AudioData")]
    fn numberOfChannels(this: &JsAudioData) -> u32;
    #[wasm_bindgen(method, getter, js_class = "AudioData")]
    fn numberOfFrames(this: &JsAudioData) -> u32;
    #[wasm_bindgen(method, getter, js_class = "AudioData")]
    fn sampleRate(this: &JsAudioData) -> f32;
    #[wasm_bindgen(method, js_class = "AudioData")]
    fn copyTo(this: &JsAudioData, dest: &JsValue, options: &JsValue);
    #[wasm_bindgen(method, js_class = "AudioData")]
    fn close(this: &JsAudioData);
}

/// Reassembles chunked DataChannel messages.
/// Small messages (≤ 16KB) arrive whole. Large messages arrive as chunks
/// with [u32 total_len LE][chunk_data] framing.
struct ChunkAssembler {
    buf: Vec<u8>,
    expected: usize,
}

impl ChunkAssembler {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            expected: 0,
        }
    }

    fn feed(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.expected == 0 {
            if data.len() < 4 {
                return Some(data.to_vec());
            }
            let total = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if total > 16_384 && total > data.len() {
                self.expected = total;
                self.buf.clear();
                self.buf.extend_from_slice(&data[4..]);
                if self.buf.len() >= self.expected {
                    self.expected = 0;
                    return Some(std::mem::take(&mut self.buf));
                }
                return None;
            }
            return Some(data.to_vec());
        }
        let payload = if data.len() >= 4 {
            let hdr = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if hdr == self.expected {
                &data[4..]
            } else {
                data
            }
        } else {
            data
        };
        self.buf.extend_from_slice(payload);
        if self.buf.len() >= self.expected {
            self.expected = 0;
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        }
    }
}

struct AppState {
    ctx: CanvasRenderingContext2d,
    canvas: HtmlCanvasElement,
    decoder: Option<JsVideoDecoder>,
    server_width: u32,
    server_height: u32,
    frame_count: u64,
    got_keyframe: bool,
    video_assembler: ChunkAssembler,
    control_assembler: ChunkAssembler,
    /// Highest sequence number from a fully rendered VideoFrame.
    /// TileUpdates with sequence <= this are stale and should be skipped.
    last_video_sequence: u64,
    /// For sending input — either DataChannel or WebSocket
    send_dc: Option<web_sys::RtcDataChannel>,
    send_ws: Option<WebSocket>,
    /// Latest stats from server (for overlay display).
    last_stats: Option<StatsSnapshot>,
    /// WebCodecs audio decoder (if server sends audio).
    audio_decoder: Option<JsAudioDecoder>,
    /// Web Audio API context for playback.
    audio_ctx: Option<web_sys::AudioContext>,
    /// Timestamp counter for audio chunks (in microseconds).
    audio_timestamp_us: i64,
}

/// Snapshot of the most recent Stats message from the server.
#[derive(Clone)]
struct StatsSnapshot {
    rtt_ms: f64,
    fps: f32,
    bandwidth_kbps: f64,
    encode_ms: f64,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

#[wasm_bindgen(start)]
pub fn main() {
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();

    // Default: WebSocket. Add ?rtc to URL for WebRTC DataChannel mode.
    let query = window.location().search().unwrap_or_default();
    let use_rtc = query.contains("rtc");

    // Extract ?token=<jwt> for authenticated connections
    let auth_token: Option<String> = query.trim_start_matches('?').split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == "token" {
            Some(v.to_string())
        } else {
            None
        }
    });
    let mode = if use_rtc { "WebRTC" } else { "WebSocket" };
    console::log_1(&format!("Phantom Web Client starting ({mode} mode)...").into());

    let canvas: HtmlCanvasElement = document
        .get_element_by_id("screen")
        .unwrap()
        .dyn_into()
        .unwrap();
    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into()
        .unwrap();

    let state = Rc::new(RefCell::new(AppState {
        ctx,
        canvas: canvas.clone(),
        decoder: None,
        server_width: 0,
        server_height: 0,
        frame_count: 0,
        got_keyframe: false,
        video_assembler: ChunkAssembler::new(),
        control_assembler: ChunkAssembler::new(),
        last_video_sequence: 0,
        send_dc: None,
        send_ws: None,
        last_stats: None,
        audio_decoder: None,
        audio_ctx: None,
        audio_timestamp_us: 0,
    }));

    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // Setup input listeners on canvas
    setup_input(&canvas, &document, &state);

    if use_rtc {
        setup_webrtc(&state);
    } else {
        setup_ws(&state, &auth_token);
    }
}

fn setup_webrtc(state: &Rc<RefCell<AppState>>) {
    use web_sys::{
        RtcConfiguration, RtcDataChannelInit, RtcPeerConnection, RtcSdpType,
        RtcSessionDescriptionInit,
    };

    let config = RtcConfiguration::new();
    let pc = match RtcPeerConnection::new_with_configuration(&config) {
        Ok(pc) => pc,
        Err(e) => {
            console::error_1(&format!("WebRTC not available: {:?}", e).into());
            return;
        }
    };

    // Create 3 DataChannels
    // Video DC: reliable + ordered (same as Parsec). SCTP handles fragmentation
    // for large keyframes. H.264 P-frames depend on previous frames, so loss
    // corrupts the entire stream — reliable delivery is correct here.
    let video_dc = pc.create_data_channel("video");
    video_dc.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);

    let input_init = RtcDataChannelInit::new();
    input_init.set_ordered(true);
    input_init.set_max_retransmits(2);
    let input_dc = pc.create_data_channel_with_data_channel_dict("input", &input_init);
    input_dc.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);

    let control_dc = pc.create_data_channel("control");
    control_dc.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);

    // Store input DC for sending
    state.borrow_mut().send_dc = Some(input_dc.clone());

    // onmessage for video DC (chunked reassembly)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let raw = js_sys::Uint8Array::new(&buf).to_vec();
                let complete = s.borrow_mut().video_assembler.feed(&raw);
                if let Some(data) = complete {
                    on_message(&s, &data);
                }
            }
        });
        video_dc.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onmessage for control DC (chunked reassembly)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let raw = js_sys::Uint8Array::new(&buf).to_vec();
                let complete = s.borrow_mut().control_assembler.feed(&raw);
                if let Some(data) = complete {
                    on_message(&s, &data);
                }
            }
        });
        control_dc.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // When control DC opens → WebRTC is fully ready
    {
        let cb = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"WebRTC DataChannels OPEN — connected!".into());
        });
        control_dc.set_onopen(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // Create offer → POST /rtc → set answer
    let pc2 = pc.clone();
    wasm_bindgen_futures::spawn_local(async move {
        // Create offer
        let offer = match wasm_bindgen_futures::JsFuture::from(pc2.create_offer()).await {
            Ok(o) => o,
            Err(e) => {
                console::error_1(&format!("createOffer: {:?}", e).into());
                return;
            }
        };
        let sdp = js_sys::Reflect::get(&offer, &"sdp".into()).unwrap();
        let sdp_str: String = sdp.as_string().unwrap_or_default();

        // Set local description
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        desc.set_sdp(&sdp_str);
        if let Err(e) = wasm_bindgen_futures::JsFuture::from(pc2.set_local_description(&desc)).await
        {
            console::error_1(&format!("setLocalDescription: {:?}", e).into());
            return;
        }
        console::log_1(&"SDP offer created, POSTing to /rtc...".into());

        // POST offer to server
        let body = serde_json::json!({ "type": "offer", "sdp": sdp_str }).to_string();
        let window = web_sys::window().unwrap();
        let request_init = web_sys::RequestInit::new();
        request_init.set_method("POST");
        request_init.set_body(&body.into());
        let headers = web_sys::Headers::new().unwrap();
        headers.set("Content-Type", "application/json").unwrap();
        request_init.set_headers(&headers);

        let resp = match wasm_bindgen_futures::JsFuture::from(
            window.fetch_with_str_and_init("/rtc", &request_init),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                console::error_1(&format!("POST /rtc failed: {:?}", e).into());
                return;
            }
        };

        let resp: web_sys::Response = resp.dyn_into().unwrap();
        if !resp.ok() {
            console::error_1(&format!("POST /rtc status: {}", resp.status()).into());
            return;
        }

        let json = match wasm_bindgen_futures::JsFuture::from(resp.json().unwrap()).await {
            Ok(j) => j,
            Err(e) => {
                console::error_1(&format!("parse answer: {:?}", e).into());
                return;
            }
        };

        // Set remote description (answer)
        let answer_sdp = js_sys::Reflect::get(&json, &"sdp".into()).unwrap();
        let answer_desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        answer_desc.set_sdp(&answer_sdp.as_string().unwrap_or_default());

        if let Err(e) =
            wasm_bindgen_futures::JsFuture::from(pc2.set_remote_description(&answer_desc)).await
        {
            console::error_1(&format!("setRemoteDescription: {:?}", e).into());
            return;
        }

        console::log_1(&"WebRTC: SDP exchange complete, waiting for ICE...".into());
    });

    // Keep PC alive
    js_sys::Reflect::set(&js_sys::global(), &"__phantom_pc".into(), &pc).unwrap();
}

fn ws_url(token: &Option<String>) -> String {
    let window = web_sys::window().unwrap();
    let location = window.location();
    let host = location.host().unwrap_or_default();
    let protocol = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    match token {
        Some(t) => format!("{protocol}://{host}/ws?token={t}"),
        None => format!("{protocol}://{host}/ws"),
    }
}

fn setup_ws(state: &Rc<RefCell<AppState>>, token: &Option<String>) {
    let url = ws_url(token);
    connect_ws(state, &url, 1000);
}

fn connect_ws(state: &Rc<RefCell<AppState>>, url: &str, retry_ms: u32) {
    console::log_1(&format!("Connecting to {url}...").into());

    let ws = match WebSocket::new(url) {
        Ok(ws) => ws,
        Err(e) => {
            console::error_1(&format!("WebSocket error: {:?}", e).into());
            schedule_reconnect(state, retry_ms);
            return;
        }
    };
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // onmessage
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                on_message(&s, &js_sys::Uint8Array::new(&buf).to_vec());
            }
        });
        ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onopen — reset retry delay
    {
        let cb = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"WebSocket connected!".into());
        });
        ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onclose — auto-reconnect with exponential backoff
    {
        let s = state.clone();
        let next_retry = (retry_ms * 2).min(5000); // cap at 5s
        let cb = Closure::<dyn FnMut()>::new(move || {
            console::warn_1(
                &format!("WebSocket closed. Reconnecting in {}ms...", next_retry).into(),
            );
            // Reset state for fresh session
            {
                let mut st = s.borrow_mut();
                st.frame_count = 0;
                st.got_keyframe = false;
                st.decoder = None;
                st.send_ws = None;
            }
            schedule_reconnect(&s, next_retry);
        });
        ws.set_onclose(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onerror
    {
        let cb = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(|_: web_sys::ErrorEvent| {
            // onclose will fire after onerror, so reconnect happens there
        });
        ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    state.borrow_mut().send_ws = Some(ws);
}

fn schedule_reconnect(state: &Rc<RefCell<AppState>>, delay_ms: u32) {
    let s = state.clone();
    let cb = Closure::<dyn FnMut()>::once(move || {
        // Re-extract token from page URL for reconnect
        let query = web_sys::window()
            .unwrap()
            .location()
            .search()
            .unwrap_or_default();
        let token: Option<String> = query.trim_start_matches('?').split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            if k == "token" {
                Some(v.to_string())
            } else {
                None
            }
        });
        let url = ws_url(&token);
        connect_ws(&s, &url, delay_ms);
    });
    let window = web_sys::window().unwrap();
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        delay_ms as i32,
    );
    cb.forget();
}

// -- Message handling (same for WebRTC and WS) --

fn on_message(state: &Rc<RefCell<AppState>>, data: &[u8]) {
    let msg: Message = match bincode::deserialize(data) {
        Ok(m) => m,
        Err(_) => return,
    };

    match msg {
        Message::Hello {
            width,
            height,
            protocol_version,
            audio,
            video_codec,
            ..
        } => {
            if protocol_version < phantom_core::protocol::MIN_PROTOCOL_VERSION {
                console::error_1(&format!(
                    "Server protocol version {protocol_version} is too old (minimum: {}). Please upgrade the server.",
                    phantom_core::protocol::MIN_PROTOCOL_VERSION
                ).into());
                return;
            }
            if protocol_version > phantom_core::protocol::PROTOCOL_VERSION {
                console::warn_1(&format!(
                    "Server is newer (v{protocol_version}) than this client (v{}). Some features may not work.",
                    phantom_core::protocol::PROTOCOL_VERSION
                ).into());
            }
            console::log_1(
                &format!("Server: {width}x{height} (protocol v{protocol_version})").into(),
            );
            let mut s = state.borrow_mut();
            s.server_width = width;
            s.server_height = height;
            s.canvas.set_width(width);
            s.canvas.set_height(height);
            drop(s);
            setup_decoder(state, width, height, video_codec);
            if audio {
                setup_audio(state);
            }
            // Send viewport size so server can match resolution (adaptive, like DCV)
            send_resolution_change(state);
        }
        Message::VideoFrame { sequence, frame } => {
            if frame.data.is_empty() {
                return;
            }
            let mut s = state.borrow_mut();

            let is_key = match frame.codec {
                VideoCodec::H264 => h264_has_idr(&frame.data),
                VideoCodec::Av1 => {
                    // AV1: first byte OBU header, check if it's a key frame
                    // For simplicity, mark first frame as key
                    s.frame_count == 0
                }
            };
            // Log first few frames for debugging
            if s.frame_count < 5 {
                let hex: String = frame
                    .data
                    .iter()
                    .take(32)
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                console::log_1(
                    &format!(
                        "frame #{}: {} bytes, kf={}, idr={}, hex=[{}]",
                        s.frame_count,
                        frame.data.len(),
                        frame.is_keyframe,
                        is_key,
                        hex
                    )
                    .into(),
                );
            }
            // WebCodecs requires a keyframe before any delta frames can be decoded.
            // Skip delta frames until we receive the first keyframe.
            if !s.got_keyframe && !is_key {
                console::warn_1(
                    &format!("skipping frame #{} (waiting for keyframe)", s.frame_count).into(),
                );
                s.frame_count += 1;
                return;
            }
            if is_key {
                s.got_keyframe = true;
            }
            s.frame_count += 1;
            s.last_video_sequence = sequence;
            let fc = s.frame_count;
            if let Some(ref decoder) = s.decoder {
                // If tab was backgrounded, decoder may be stale. Reset on keyframe.
                if is_key {
                    let state_str = js_sys::Reflect::get(decoder.as_ref(), &"state".into())
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_default();
                    if state_str == "closed" {
                        console::warn_1(&"Decoder closed, skipping frame".into());
                        return;
                    }
                }
                let data_js = js_sys::Uint8Array::from(frame.data.as_slice());
                let init = js_sys::Object::new();
                js_sys::Reflect::set(
                    &init,
                    &"type".into(),
                    &if is_key { "key" } else { "delta" }.into(),
                )
                .unwrap();
                js_sys::Reflect::set(&init, &"timestamp".into(), &(fc as f64 * 33333.0).into())
                    .unwrap();
                js_sys::Reflect::set(&init, &"data".into(), &data_js.buffer()).unwrap();
                let chunk = JsEncodedVideoChunk::new(&init);
                decoder.decode(&chunk);
            }
        }
        Message::TileUpdate { sequence, tiles } => {
            let s = state.borrow();
            // Skip tile updates that are older than the last full video frame,
            // since the video frame already contains the complete screen state.
            if sequence <= s.last_video_sequence {
                return;
            }
            for tile in tiles.iter() {
                let bgra = match tile.encoding {
                    TileEncoding::Zstd => {
                        let mut dec = match ruzstd::StreamingDecoder::new(tile.data.as_slice()) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };
                        let mut out = Vec::new();
                        if std::io::Read::read_to_end(&mut dec, &mut out).is_err() {
                            continue;
                        }
                        out
                    }
                    TileEncoding::Raw => tile.data.clone(),
                    _ => continue,
                };
                let tw = tile.pixel_width as usize;
                let th = tile.pixel_height as usize;
                if bgra.len() < tw * th * 4 {
                    continue;
                }
                let mut rgba = vec![0u8; tw * th * 4];
                for i in 0..tw * th {
                    rgba[i * 4] = bgra[i * 4 + 2];
                    rgba[i * 4 + 1] = bgra[i * 4 + 1];
                    rgba[i * 4 + 2] = bgra[i * 4];
                    rgba[i * 4 + 3] = 255;
                }
                let clamped = wasm_bindgen::Clamped(&rgba[..]);
                if let Ok(img) = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
                    clamped, tw as u32, th as u32,
                ) {
                    let _ = s.ctx.put_image_data(
                        &img,
                        (tile.tile_x * TILE_SIZE) as f64,
                        (tile.tile_y * TILE_SIZE) as f64,
                    );
                }
            }
        }
        Message::ClipboardSync(text) => {
            // Write remote clipboard to local clipboard via Async Clipboard API.
            // Requires HTTPS (secure context) and document focus.
            if let Some(w) = web_sys::window() {
                if w.is_secure_context() {
                    let cb = w.navigator().clipboard();
                    wasm_bindgen_futures::spawn_local(async move {
                        match wasm_bindgen_futures::JsFuture::from(cb.write_text(&text)).await {
                            Ok(_) => {
                                console::log_1(&"clipboard: synced from server".into());
                            }
                            Err(_e) => {
                                // Permission denied or document not focused — silent fail
                            }
                        }
                    });
                }
            }
        }
        Message::AudioFrame { data, .. } => {
            let mut s = state.borrow_mut();
            if s.audio_decoder.is_none() {
                return;
            }
            // Don't decode while AudioContext is suspended — frames would pile up
            if let Some(ref ctx) = s.audio_ctx {
                if ctx.state() == web_sys::AudioContextState::Suspended {
                    return; // drop frame, will get fresh data after resume
                }
            }

            let timestamp = s.audio_timestamp_us;
            s.audio_timestamp_us += 20_000; // 20ms per Opus frame

            let js_data = js_sys::Uint8Array::from(data.as_slice());
            let init = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&init, &"type".into(), &"key".into());
            let _ = js_sys::Reflect::set(&init, &"timestamp".into(), &(timestamp as f64).into());
            let _ = js_sys::Reflect::set(&init, &"data".into(), &js_data.buffer());
            let chunk = JsEncodedAudioChunk::new(&init);
            s.audio_decoder.as_ref().unwrap().decode(&chunk);
        }
        Message::Ping => {
            let s = state.borrow();
            send_message(&s, &Message::Pong);
        }
        Message::Stats {
            rtt_us,
            fps,
            bandwidth_bps,
            encode_us,
        } => {
            state.borrow_mut().last_stats = Some(StatsSnapshot {
                rtt_ms: rtt_us as f64 / 1000.0,
                fps,
                bandwidth_kbps: bandwidth_bps as f64 / 1024.0,
                encode_ms: encode_us as f64 / 1000.0,
            });
        }
        _ => {}
    }
}

/// Check if an H.264 bitstream contains an IDR (keyframe) NAL unit.
fn h264_has_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < data.len() {
        // Look for start code 00 00 00 01 or 00 00 01
        if data[i..i + 4] == [0, 0, 0, 1] {
            let nal_type = data[i + 4] & 0x1f;
            // 5 = IDR slice, 7 = SPS (always precedes keyframe)
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 4;
        } else if i + 3 < data.len() && data[i..i + 3] == [0, 0, 1] {
            let nal_type = data[i + 3] & 0x1f;
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

fn create_decoder_config(
    width: u32,
    height: u32,
    codec: VideoCodec,
    hw_accel: &str,
) -> js_sys::Object {
    let config = js_sys::Object::new();
    let codec_str = match codec {
        VideoCodec::Av1 => "av01.0.08M.08",
        _ => "avc1.42c028",
    };
    js_sys::Reflect::set(&config, &"codec".into(), &codec_str.into()).unwrap();
    js_sys::Reflect::set(&config, &"codedWidth".into(), &(width).into()).unwrap();
    js_sys::Reflect::set(&config, &"codedHeight".into(), &(height).into()).unwrap();
    js_sys::Reflect::set(&config, &"optimizeForLatency".into(), &true.into()).unwrap();
    js_sys::Reflect::set(&config, &"hardwareAcceleration".into(), &hw_accel.into()).unwrap();
    config
}

fn setup_decoder(state: &Rc<RefCell<AppState>>, width: u32, height: u32, codec: VideoCodec) {
    let s = state.clone();
    let decode_count = Rc::new(RefCell::new(0u64));
    let dc = decode_count.clone();

    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |frame: JsValue| {
        let mut count = dc.borrow_mut();
        *count += 1;

        // Read actual frame dimensions (may differ after resolution change).
        // WebCodecs VideoFrame has displayWidth/displayHeight properties.
        let fw = js_sys::Reflect::get(&frame, &"displayWidth".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u32;
        let fh = js_sys::Reflect::get(&frame, &"displayHeight".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u32;

        let mut st = s.borrow_mut();
        // Detect resolution change from decoded frame — update canvas + mapping
        if fw > 0 && fh > 0 && (fw != st.server_width || fh != st.server_height) {
            console::log_1(
                &format!("Resolution changed: {}x{} → {fw}x{fh}", st.server_width, st.server_height).into(),
            );
            st.server_width = fw;
            st.server_height = fh;
            st.canvas.set_width(fw);
            st.canvas.set_height(fh);
        }

        let w = st.server_width;
        let h = st.server_height;
        drop(st);

        js_sys::Reflect::set(&js_sys::global(), &"__phantom_frame".into(), &frame).unwrap();
        let js_code = format!(
            "var c=document.getElementById('screen').getContext('2d'); c.drawImage(__phantom_frame, 0, 0, {w}, {h}); __phantom_frame.close();"
        );
        js_sys::eval(&js_code).unwrap_or(JsValue::NULL);
    });

    let error_cb = Closure::<dyn FnMut(JsValue)>::new(|e: JsValue| {
        console::error_1(&format!("Decode error: {:?}", e).into());
    });

    let init = js_sys::Object::new();
    js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref()).unwrap();
    js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref()).unwrap();
    let decoder = JsVideoDecoder::new(&init);

    // Use software decode: Chrome's hardware WebCodecs decoder defers output
    // callback when tab isn't fully focused (after URL navigation), causing
    // black screen until click. Software decode (~2-4ms/frame at 1080p) is
    // fast enough and guarantees immediate first frame. The bottleneck is
    // network RTT (~100ms), not decode time.
    let config = create_decoder_config(width, height, codec, "prefer-software");
    decoder.configure(&config);

    state.borrow_mut().decoder = Some(decoder);
    let codec_name = match codec {
        VideoCodec::Av1 => "AV1",
        _ => "H.264",
    };
    console::log_1(&format!("{codec_name} decoder ready").into());
    output_cb.forget();
    error_cb.forget();
}

// -- Audio --

/// Initialize WebCodecs AudioDecoder + Web Audio API for Opus playback.
fn setup_audio(state: &Rc<RefCell<AppState>>) {
    // Open dedicated audio WebSocket (independent from video WS)
    let window = web_sys::window().unwrap();
    let location = window.location();
    let host = location.host().unwrap_or_default();
    let protocol = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    // Include auth token in audio WS URL if present
    let query = location.search().unwrap_or_default();
    let token: Option<String> = query.trim_start_matches('?').split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == "token" {
            Some(v.to_string())
        } else {
            None
        }
    });
    let audio_url = match &token {
        Some(t) => format!("{protocol}://{host}/ws/audio?token={t}"),
        None => format!("{protocol}://{host}/ws/audio"),
    };

    let s = state.clone();
    match WebSocket::new(&audio_url) {
        Ok(ws) => {
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
            let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                    on_message(&s, &js_sys::Uint8Array::new(&buf).to_vec());
                }
            });
            ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
            console::log_1(&format!("audio WebSocket connected to {audio_url}").into());
        }
        Err(e) => {
            console::warn_1(
                &format!("audio WebSocket failed ({e:?}), using main WS fallback").into(),
            );
        }
    }

    // Create AudioContext at 48kHz (must match Opus output to avoid resampling stutter)
    let ctx_opts = web_sys::AudioContextOptions::new();
    ctx_opts.set_sample_rate(48000.0);
    ctx_opts.set_latency_hint(&wasm_bindgen::JsValue::from_str("interactive"));
    let audio_ctx = match web_sys::AudioContext::new_with_context_options(&ctx_opts) {
        Ok(ctx) => ctx,
        Err(e) => {
            console::warn_1(&format!("AudioContext creation failed: {e:?}").into());
            return;
        }
    };
    console::log_1(&format!("AudioContext: sampleRate={}", audio_ctx.sample_rate()).into());

    // Create a ring buffer for decoded PCM samples.
    // The AudioWorklet reads from this buffer at a steady rate (128 samples per
    // render quantum at 48 kHz ≈ 2.67ms), while the main thread pushes decoded
    // Opus frames (~960 samples at 48 kHz = 20ms). The buffer holds ~200ms of
    // audio to absorb network jitter.
    //
    // We use a SharedArrayBuffer so the worklet can read without message passing.
    // Fallback: if SharedArrayBuffer is unavailable (cross-origin isolation not
    // set), we fall back to the old BufferSourceNode approach.

    let _ring_size: u32 = 48000 * 3 / 10;
    let _channels: u32 = 2;

    // Try SharedArrayBuffer for zero-copy AudioWorklet (pull-based, lowest latency).
    // Falls back to BufferSourceNode jitter buffer if SAB unavailable.
    let sab_available = js_sys::Reflect::get(&js_sys::global(), &"SharedArrayBuffer".into())
        .map(|v| v.is_function())
        .unwrap_or(false);

    if sab_available {
        setup_audio_sab_worklet(state, &audio_ctx);
    } else {
        console::warn_1(&"SharedArrayBuffer unavailable — using BufferSourceNode fallback".into());
        setup_audio_buffersource(state, &audio_ctx);
    }
}

/// Pull-based audio via AudioWorklet + SharedArrayBuffer (zero-copy, lowest latency).
// Old MessagePort-based AudioWorklet (replaced by SAB approach below).
#[allow(dead_code)]
fn _setup_audio_worklet(
    state: &Rc<RefCell<AppState>>,
    audio_ctx: &web_sys::AudioContext,
    ring_size: u32,
    channels: u32,
) {
    // AudioWorklet processor code (inlined as Blob URL)
    let worklet_code = r#"
class PhantomAudioProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.ringSize = options.processorOptions.ringSize || 14400;
    this.channels = options.processorOptions.channels || 2;
    this.ring = [];
    for (let i = 0; i < this.channels; i++) {
      this.ring.push(new Float32Array(this.ringSize));
    }
    this.writePos = 0;
    this.readPos = 0;
    this.buffered = 0;
    this.started = false;

    // Adaptive jitter buffer: starts at 40ms, grows on underflow, shrinks when stable
    this.minPrefill = Math.floor(48000 * 0.02);  // 20ms floor
    this.maxPrefill = Math.floor(48000 * 0.2);   // 200ms ceiling
    this.prefill = Math.floor(48000 * 0.04);      // start at 40ms
    this.stepUp = Math.floor(48000 * 0.02);       // grow by 20ms per underflow
    this.stepDown = Math.floor(48000 * 0.01);     // shrink by 10ms
    this.underflowCount = 0;
    this.stableCount = 0;
    // Shrink after ~5 seconds of stability (5s / 2.67ms per process call ≈ 1875)
    this.stableThreshold = 1875;

    this.port.onmessage = (e) => {
      const { channelData } = e.data;
      if (!channelData || !channelData[0]) return;
      const frames = channelData[0].length;
      for (let ch = 0; ch < Math.min(this.channels, channelData.length); ch++) {
        const src = channelData[ch];
        for (let i = 0; i < frames; i++) {
          this.ring[ch][(this.writePos + i) % this.ringSize] = src[i];
        }
      }
      this.writePos = (this.writePos + frames) % this.ringSize;
      this.buffered += frames;
      if (this.buffered > this.ringSize) this.buffered = this.ringSize;
    };
  }

  process(inputs, outputs, parameters) {
    const output = outputs[0];
    const frames = output[0].length;

    // Wait until prefill threshold reached
    if (!this.started) {
      if (this.buffered < this.prefill) {
        for (let ch = 0; ch < output.length; ch++) output[ch].fill(0);
        return true;
      }
      this.started = true;
      this.stableCount = 0;
    }

    // Underflow: grow buffer, reset to re-prefill
    if (this.buffered < frames) {
      this.underflowCount++;
      this.prefill = Math.min(this.prefill + this.stepUp, this.maxPrefill);
      this.started = false;
      for (let ch = 0; ch < output.length; ch++) output[ch].fill(0);
      return true;
    }

    // Stable playback: count towards shrink
    this.stableCount++;
    if (this.stableCount >= this.stableThreshold && this.prefill > this.minPrefill) {
      this.prefill = Math.max(this.prefill - this.stepDown, this.minPrefill);
      this.stableCount = 0;
    }

    // Overflow protection: if buffer is way too full (>80%), skip ahead
    // to prevent latency from growing unbounded
    const maxBuffered = Math.floor(this.ringSize * 0.8);
    if (this.buffered > maxBuffered) {
      const skip = this.buffered - this.prefill;
      this.readPos = (this.readPos + skip) % this.ringSize;
      this.buffered -= skip;
    }

    for (let ch = 0; ch < output.length; ch++) {
      const ring = this.ring[Math.min(ch, this.channels - 1)];
      for (let i = 0; i < frames; i++) {
        output[ch][i] = ring[(this.readPos + i) % this.ringSize];
      }
    }
    this.readPos = (this.readPos + frames) % this.ringSize;
    this.buffered -= frames;
    return true;
  }
}
registerProcessor('phantom-audio', PhantomAudioProcessor);
"#;

    // Create Blob URL for the worklet
    let blob_parts = js_sys::Array::new();
    blob_parts.push(&worklet_code.into());
    let blob_opts = web_sys::BlobPropertyBag::new();
    blob_opts.set_type("application/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_opts).unwrap();
    let url = web_sys::Url::create_object_url_with_blob(&blob).unwrap();

    // Load the worklet module
    let ctx_clone = audio_ctx.clone();
    let state_clone = state.clone();
    let url_clone = url.clone();
    let ring_size_val = ring_size;
    let channels_val = channels;

    let promise = audio_ctx.audio_worklet().unwrap().add_module(&url).unwrap();
    let on_loaded = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
        // Revoke the blob URL
        let _ = web_sys::Url::revoke_object_url(&url_clone);

        // Create the AudioWorkletNode
        let opts = js_sys::Object::new();
        let proc_opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&proc_opts, &"ringSize".into(), &ring_size_val.into());
        let _ = js_sys::Reflect::set(&proc_opts, &"channels".into(), &channels_val.into());
        let _ = js_sys::Reflect::set(&opts, &"processorOptions".into(), &proc_opts);
        let _ = js_sys::Reflect::set(&opts, &"numberOfInputs".into(), &0.into());
        let _ = js_sys::Reflect::set(&opts, &"numberOfOutputs".into(), &1.into());
        let output_channels = js_sys::Array::new();
        output_channels.push(&channels_val.into());
        let _ = js_sys::Reflect::set(&opts, &"outputChannelCount".into(), &output_channels);

        let node = web_sys::AudioWorkletNode::new_with_options(
            &ctx_clone,
            "phantom-audio",
            &opts.unchecked_into(),
        )
        .unwrap();
        let _ = node.connect_with_audio_node(&ctx_clone.destination());

        console::log_1(&"AudioWorklet ring-buffer playback initialized".into());

        // Store the worklet node's port for sending decoded samples
        let port = node.port().unwrap();

        // Set up AudioDecoder with output → worklet port
        _setup_audio_decoder_worklet(&state_clone, &ctx_clone, port);
    });

    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
        console::warn_1(&format!("AudioWorklet load failed: {e:?}").into());
    });

    let _ = promise.then2(&on_loaded, &on_error);
    on_loaded.forget();
    on_error.forget();
}

#[allow(dead_code)]
fn _setup_audio_decoder_worklet(
    state: &Rc<RefCell<AppState>>,
    audio_ctx: &web_sys::AudioContext,
    port: web_sys::MessagePort,
) {
    let port_clone = port.clone();
    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |output: JsValue| {
        let audio_data: JsAudioData = output.unchecked_into();
        let channels = audio_data.numberOfChannels();
        let frames = audio_data.numberOfFrames();

        if frames == 0 || channels == 0 {
            audio_data.close();
            return;
        }

        // Copy decoded PCM into Float32Arrays and send to worklet
        let channel_data = js_sys::Array::new();
        for ch in 0..channels {
            let data = js_sys::Float32Array::new_with_length(frames);
            let opts = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&opts, &"planeIndex".into(), &ch.into());
            let _ = js_sys::Reflect::set(&opts, &"format".into(), &"f32-planar".into());
            audio_data.copyTo(&data, &opts);
            channel_data.push(&data);
        }
        audio_data.close();

        // Send to worklet (transfers ownership of the underlying ArrayBuffers)
        let msg = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&msg, &"channelData".into(), &channel_data);
        let _ = port_clone.post_message(&msg);
    });

    let error_cb = Closure::<dyn FnMut(JsValue)>::new(|e: JsValue| {
        console::warn_1(&format!("AudioDecoder error: {e:?}").into());
    });

    let init = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref());
    let _ = js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref());
    let audio_decoder = JsAudioDecoder::new(&init);

    // Configure for Opus 48kHz stereo
    let config = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&config, &"codec".into(), &"opus".into());
    let _ = js_sys::Reflect::set(&config, &"sampleRate".into(), &48000.into());
    let _ = js_sys::Reflect::set(&config, &"numberOfChannels".into(), &2.into());
    audio_decoder.configure(&config);

    {
        let mut s = state.borrow_mut();
        s.audio_decoder = Some(audio_decoder);
        s.audio_ctx = Some(audio_ctx.clone());
    }

    output_cb.forget();
    error_cb.forget();
}

/// Pull-based audio via AudioWorklet + SharedArrayBuffer (zero-copy, lowest latency).
/// Main thread writes decoded PCM directly to shared memory. AudioWorklet reads it.
/// No MessagePort, no structured clone, no GC pressure.
fn setup_audio_sab_worklet(state: &Rc<RefCell<AppState>>, audio_ctx: &web_sys::AudioContext) {
    // Ring buffer: 500ms stereo float32 = 48000 * 0.5 * 2 * 4 = 192000 bytes
    let ring_samples: u32 = 48000 / 2; // 24000 samples per channel (500ms)
    let channels: u32 = 2;

    // SharedArrayBuffer for control: [writePos, readPos, buffered] as Uint32
    let ctrl_sab = js_sys::SharedArrayBuffer::new(3 * 4); // 3 x u32
    let ctrl_main = js_sys::Uint32Array::new(&ctrl_sab);

    // SharedArrayBuffer for audio: interleaved stereo float32
    let audio_sab = js_sys::SharedArrayBuffer::new(ring_samples * channels * 4);
    let audio_main = js_sys::Float32Array::new(&audio_sab);

    // AudioWorklet processor code
    let worklet_code = r#"
class PhantomSABProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { ctrlBuffer, audioBuffer, ringSize, channels } = options.processorOptions;
    this.ctrl = new Uint32Array(ctrlBuffer);   // [writePos, readPos, buffered]
    this.audio = new Float32Array(audioBuffer); // interleaved stereo
    this.ringSize = ringSize;
    this.channels = channels;
    // Pre-buffer 60ms before starting
    this.prefill = Math.floor(48000 * 0.06);
    this.started = false;
  }

  process(inputs, outputs, parameters) {
    const output = outputs[0];
    const frames = output[0].length; // 128
    const buffered = Atomics.load(this.ctrl, 2);

    if (!this.started) {
      if (buffered < this.prefill) {
        for (let ch = 0; ch < output.length; ch++) output[ch].fill(0);
        return true;
      }
      this.started = true;
    }

    if (buffered < frames) {
      // Underflow — silence, re-prefill
      for (let ch = 0; ch < output.length; ch++) output[ch].fill(0);
      this.started = false;
      return true;
    }

    let readPos = Atomics.load(this.ctrl, 1);
    for (let i = 0; i < frames; i++) {
      const idx = ((readPos + i) % this.ringSize) * this.channels;
      for (let ch = 0; ch < Math.min(output.length, this.channels); ch++) {
        output[ch][i] = this.audio[idx + ch];
      }
    }
    const newReadPos = (readPos + frames) % this.ringSize;
    Atomics.store(this.ctrl, 1, newReadPos);
    Atomics.sub(this.ctrl, 2, frames);
    return true;
  }
}
registerProcessor('phantom-sab-audio', PhantomSABProcessor);
"#;

    let blob_parts = js_sys::Array::new();
    blob_parts.push(&worklet_code.into());
    let blob_opts = web_sys::BlobPropertyBag::new();
    blob_opts.set_type("application/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&blob_parts, &blob_opts).unwrap();
    let url = web_sys::Url::create_object_url_with_blob(&blob).unwrap();

    let ctx_clone = audio_ctx.clone();
    let state_clone = state.clone();
    let url_clone = url.clone();

    // Store SAB refs for the AudioDecoder callback to write into
    let ctrl_write = ctrl_main.clone();
    let audio_write = audio_main.clone();
    let ring_size = ring_samples;

    let promise = audio_ctx.audio_worklet().unwrap().add_module(&url).unwrap();
    let on_loaded = Closure::<dyn FnMut(JsValue)>::once(move |_: JsValue| {
        let _ = web_sys::Url::revoke_object_url(&url_clone);

        let opts = js_sys::Object::new();
        let proc_opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&proc_opts, &"ctrlBuffer".into(), &ctrl_sab);
        let _ = js_sys::Reflect::set(&proc_opts, &"audioBuffer".into(), &audio_sab);
        let _ = js_sys::Reflect::set(&proc_opts, &"ringSize".into(), &ring_size.into());
        let _ = js_sys::Reflect::set(&proc_opts, &"channels".into(), &channels.into());
        let _ = js_sys::Reflect::set(&opts, &"processorOptions".into(), &proc_opts);
        let _ = js_sys::Reflect::set(&opts, &"numberOfInputs".into(), &0.into());
        let _ = js_sys::Reflect::set(&opts, &"numberOfOutputs".into(), &1.into());
        let out_ch = js_sys::Array::new();
        out_ch.push(&channels.into());
        let _ = js_sys::Reflect::set(&opts, &"outputChannelCount".into(), &out_ch);

        let node = web_sys::AudioWorkletNode::new_with_options(
            &ctx_clone,
            "phantom-sab-audio",
            &opts.unchecked_into(),
        )
        .unwrap();
        let _ = node.connect_with_audio_node(&ctx_clone.destination());

        // Set up AudioDecoder that writes directly to SharedArrayBuffer
        setup_audio_decoder_sab(&state_clone, &ctx_clone, ctrl_write, audio_write, ring_size);

        console::log_1(&"AudioWorklet pull-based playback initialized (SharedArrayBuffer)".into());
    });

    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
        console::warn_1(&format!("AudioWorklet load failed: {e:?}").into());
    });

    let _ = promise.then2(&on_loaded, &on_error);
    on_loaded.forget();
    on_error.forget();
}

/// AudioDecoder that writes decoded PCM directly into SharedArrayBuffer.
fn setup_audio_decoder_sab(
    state: &Rc<RefCell<AppState>>,
    audio_ctx: &web_sys::AudioContext,
    ctrl: js_sys::Uint32Array,
    audio: js_sys::Float32Array,
    ring_size: u32,
) {
    let channels: u32 = 2;

    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |output: JsValue| {
        let audio_data: JsAudioData = output.unchecked_into();
        let dec_channels = audio_data.numberOfChannels();
        let frames = audio_data.numberOfFrames();

        if frames == 0 || dec_channels == 0 {
            audio_data.close();
            return;
        }

        // Extract planar float32
        let mut left = vec![0f32; frames as usize];
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&opts, &"planeIndex".into(), &0u32.into());
        let _ = js_sys::Reflect::set(&opts, &"format".into(), &"f32-planar".into());
        let arr = js_sys::Float32Array::new_with_length(frames);
        audio_data.copyTo(&arr, &opts);
        arr.copy_to(&mut left);

        let right = if dec_channels >= 2 {
            let mut r = vec![0f32; frames as usize];
            let opts_r = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&opts_r, &"planeIndex".into(), &1u32.into());
            let _ = js_sys::Reflect::set(&opts_r, &"format".into(), &"f32-planar".into());
            let arr_r = js_sys::Float32Array::new_with_length(frames);
            audio_data.copyTo(&arr_r, &opts_r);
            arr_r.copy_to(&mut r);
            r
        } else {
            left.clone()
        };
        audio_data.close();

        // Write interleaved samples directly to SharedArrayBuffer
        // Use Atomics for writePos synchronization
        let write_pos = js_sys::Atomics::load(&ctrl, 0).unwrap_or(0) as u32;
        let mut interleaved = vec![0f32; frames as usize * channels as usize];
        for i in 0..frames as usize {
            interleaved[i * 2] = left[i];
            interleaved[i * 2 + 1] = right[i];
        }

        // Write to ring buffer with wrap-around
        for i in 0..frames as usize {
            let idx = ((write_pos as usize + i) % ring_size as usize) * channels as usize;
            audio.set_index(idx as u32, interleaved[i * 2]);
            audio.set_index(idx as u32 + 1, interleaved[i * 2 + 1]);
        }

        let new_write_pos = (write_pos + frames) % ring_size;
        let _ = js_sys::Atomics::store(&ctrl, 0, new_write_pos as i32);
        let _ = js_sys::Atomics::add(&ctrl, 2, frames as i32);

        // Cap buffered to ring_size (overflow protection)
        let buffered = js_sys::Atomics::load(&ctrl, 2).unwrap_or(0) as u32;
        if buffered > ring_size {
            let _ = js_sys::Atomics::store(&ctrl, 2, ring_size as i32);
        }
    });

    let error_cb = Closure::<dyn FnMut(JsValue)>::new(|e: JsValue| {
        console::warn_1(&format!("AudioDecoder error: {e:?}").into());
    });

    let init = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref());
    let _ = js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref());
    let audio_decoder = JsAudioDecoder::new(&init);

    let config = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&config, &"codec".into(), &"opus".into());
    let _ = js_sys::Reflect::set(&config, &"sampleRate".into(), &48000.into());
    let _ = js_sys::Reflect::set(&config, &"numberOfChannels".into(), &2.into());
    audio_decoder.configure(&config);

    {
        let mut s = state.borrow_mut();
        s.audio_decoder = Some(audio_decoder);
        s.audio_ctx = Some(audio_ctx.clone());
    }

    output_cb.forget();
    error_cb.forget();
}

/// Audio playback using a jitter buffer + periodic drain.
/// Decoded PCM accumulates in a buffer. A 40ms timer drains it into
/// BufferSourceNodes, smoothing out bursty AudioDecoder callbacks.
fn setup_audio_buffersource(state: &Rc<RefCell<AppState>>, audio_ctx: &web_sys::AudioContext) {
    let _ctx_clone = audio_ctx.clone();
    let ctx_drain = audio_ctx.clone();

    // Jitter buffer: accumulates decoded PCM (interleaved stereo f32)
    let pcm_buf: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::with_capacity(48000))); // ~500ms
    let pcm_buf_write = pcm_buf.clone();
    let pcm_buf_drain = pcm_buf.clone();
    let next_time = Rc::new(RefCell::new(0.0f64));
    let next_time_drain = next_time.clone();

    // AudioDecoder output callback: just accumulates PCM, doesn't schedule
    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |output: JsValue| {
        let audio_data: JsAudioData = output.unchecked_into();
        let channels = audio_data.numberOfChannels();
        let frames = audio_data.numberOfFrames();

        if frames == 0 || channels == 0 {
            audio_data.close();
            return;
        }

        // Extract interleaved stereo f32
        let mut left = vec![0f32; frames as usize];
        let mut right = vec![0f32; frames as usize];
        let opts_l = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&opts_l, &"planeIndex".into(), &0u32.into());
        let _ = js_sys::Reflect::set(&opts_l, &"format".into(), &"f32-planar".into());
        let left_arr = js_sys::Float32Array::new_with_length(frames);
        audio_data.copyTo(&left_arr, &opts_l);
        left_arr.copy_to(&mut left);

        if channels >= 2 {
            let opts_r = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&opts_r, &"planeIndex".into(), &1u32.into());
            let _ = js_sys::Reflect::set(&opts_r, &"format".into(), &"f32-planar".into());
            let right_arr = js_sys::Float32Array::new_with_length(frames);
            audio_data.copyTo(&right_arr, &opts_r);
            right_arr.copy_to(&mut right);
        } else {
            right = left.clone();
        }
        audio_data.close();

        // Append interleaved samples to jitter buffer
        let mut buf = pcm_buf_write.borrow_mut();
        for i in 0..frames as usize {
            buf.push(left[i]);
            buf.push(right[i]);
        }
        // Cap buffer at 500ms (48000 samples * 2 channels)
        if buf.len() > 48000 * 2 {
            let excess = buf.len() - 48000 * 2;
            buf.drain(..excess);
        }
    });

    // Periodic drain: every 40ms, take accumulated PCM and schedule playback
    let drain_cb = Closure::<dyn FnMut()>::new(move || {
        let mut buf = pcm_buf_drain.borrow_mut();
        if buf.is_empty() {
            return;
        }

        let current_time = ctx_drain.current_time();
        let mut scheduled = next_time_drain.borrow_mut();

        // Reset if behind or way ahead
        if *scheduled < current_time {
            *scheduled = current_time + 0.08; // 80ms buffer to absorb jitter
        }
        if *scheduled > current_time + 0.4 {
            *scheduled = current_time + 0.08;
            buf.clear(); // drop stale audio
            return;
        }

        // Drain all accumulated samples into one AudioBuffer
        let total_samples = buf.len() / 2; // stereo interleaved
        if total_samples == 0 {
            return;
        }

        let buffer = match ctx_drain.create_buffer(2, total_samples as u32, 48000.0) {
            Ok(b) => b,
            Err(_) => return,
        };

        if let Ok(mut left_data) = buffer.get_channel_data(0) {
            for i in 0..total_samples {
                left_data[i] = buf[i * 2];
            }
            let _ = buffer.copy_to_channel(&left_data, 0);
        }
        if let Ok(mut right_data) = buffer.get_channel_data(1) {
            for i in 0..total_samples {
                right_data[i] = buf[i * 2 + 1];
            }
            let _ = buffer.copy_to_channel(&right_data, 1);
        }
        buf.clear();

        if let Ok(source) = ctx_drain.create_buffer_source() {
            source.set_buffer(Some(&buffer));
            let _ = source.connect_with_audio_node(&ctx_drain.destination());
            let _ = source.start_with_when(*scheduled);
            *scheduled += total_samples as f64 / 48000.0;
        }
    });

    // Start 20ms drain timer (matches Opus frame duration for smooth playback)
    let window = web_sys::window().unwrap();
    let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
        drain_cb.as_ref().unchecked_ref(),
        20,
    );
    drain_cb.forget();

    let error_cb = Closure::<dyn FnMut(JsValue)>::new(|e: JsValue| {
        console::warn_1(&format!("AudioDecoder error: {e:?}").into());
    });

    let init = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref());
    let _ = js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref());
    let audio_decoder = JsAudioDecoder::new(&init);

    // Configure for Opus 48kHz stereo
    let config = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&config, &"codec".into(), &"opus".into());
    let _ = js_sys::Reflect::set(&config, &"sampleRate".into(), &48000.into());
    let _ = js_sys::Reflect::set(&config, &"numberOfChannels".into(), &2.into());
    audio_decoder.configure(&config);

    {
        let mut s = state.borrow_mut();
        s.audio_decoder = Some(audio_decoder);
        s.audio_ctx = Some(audio_ctx.clone());
    }

    output_cb.forget();
    error_cb.forget();

    console::log_1(&"Audio playback initialized (BufferSourceNode fallback)".into());
}

// -- Input --

fn setup_input(
    canvas: &HtmlCanvasElement,
    document: &web_sys::Document,
    state: &Rc<RefCell<AppState>>,
) {
    // Resume AudioContext on first user interaction (browser autoplay policy)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            let st = s.borrow();
            if let Some(ref ctx) = st.audio_ctx {
                if ctx.state() == web_sys::AudioContextState::Suspended {
                    let _ = ctx.resume();
                    console::log_1(&"AudioContext resumed after user gesture".into());
                }
            }
        });
        let _ = document.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        let _ = document.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            let st = s.borrow();
            if st.server_width == 0 || st.server_height == 0 {
                return;
            }
            let (x, y) = map_mouse(
                &st.canvas,
                e.client_x() as f64,
                e.client_y() as f64,
                st.server_width,
                st.server_height,
            );
            send_input(&st, InputEvent::MouseMove { x, y });
        });
        canvas
            .add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    for name in &["mousedown", "mouseup"] {
        let s = state.clone();
        let pressed = *name == "mousedown";
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.prevent_default();
            let st = s.borrow();
            let btn = match e.button() {
                0 => MouseButton::Left,
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                _ => return,
            };
            send_input(
                &st,
                InputEvent::MouseButton {
                    button: btn,
                    pressed,
                },
            );
        });
        canvas
            .add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    {
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(|e: MouseEvent| e.prevent_default());
        canvas
            .add_event_listener_with_callback("contextmenu", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    {
        let s = state.clone();
        // Scroll: accumulate pixel deltas per rAF frame, convert to line counts.
        // Uses Sunshine-style accumulation: fractional deltas build up until they
        // reach a full "notch" (120 pixels = 1 discrete scroll unit).
        // Direction follows the client's native behavior (browser already applies
        // macOS natural scroll, etc.) — we just negate to match X11/enigo convention.
        let scroll_accum = Rc::new(RefCell::new((0.0f64, 0.0f64)));
        let scroll_raf_pending = Rc::new(RefCell::new(false));
        let scroll_accum2 = scroll_accum.clone();
        let scroll_raf2 = scroll_raf_pending.clone();
        let s2 = s.clone();
        let cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
            e.prevent_default();

            // Pass through client's scroll direction as-is.
            // Browser deltaY already reflects the client OS settings (macOS natural
            // scroll, etc.). Positive = scroll down, which maps to enigo ScrollDown.
            let mut acc = scroll_accum.borrow_mut();
            acc.0 += e.delta_x() as f64;
            acc.1 += e.delta_y() as f64;
            drop(acc);

            // Schedule flush on next rAF (one flush per frame, ~60Hz)
            if !*scroll_raf_pending.borrow() {
                *scroll_raf_pending.borrow_mut() = true;
                let sa = scroll_accum2.clone();
                let sp = scroll_raf2.clone();
                let ss = s2.clone();
                let flush = Closure::<dyn FnMut(f64)>::once(move |_: f64| {
                    let mut acc = sa.borrow_mut();
                    // Convert pixel delta to line counts.
                    // Mouse wheel: 120px per notch → 1 line.
                    // Trackpad: small deltas accumulate across frames.
                    let lines_x = (acc.0 / 120.0).trunc();
                    let lines_y = (acc.1 / 120.0).trunc();
                    // Keep the remainder for next frame (smooth sub-notch accumulation)
                    acc.0 -= lines_x * 120.0;
                    acc.1 -= lines_y * 120.0;
                    drop(acc);

                    if lines_x != 0.0 || lines_y != 0.0 {
                        let st = ss.borrow();
                        send_input(
                            &st,
                            InputEvent::MouseScroll {
                                dx: lines_x as f32,
                                dy: lines_y as f32,
                            },
                        );
                    }
                    *sp.borrow_mut() = false;
                });
                let window = web_sys::window().unwrap();
                let _ = window.request_animation_frame(flush.as_ref().unchecked_ref());
                flush.forget();
            }
        });
        canvas
            .add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    for name in &["keydown", "keyup"] {
        let s = state.clone();
        let pressed = *name == "keydown";
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            e.prevent_default();
            let st = s.borrow();
            let code = e.code();
            if code == "MetaLeft" || code == "MetaRight" {
                return;
            }
            // Paste: Cmd+V / Ctrl+V — must check BEFORE the meta_key() guard
            if pressed && code == "KeyV" && (e.ctrl_key() || e.meta_key()) {
                if let Some(w) = web_sys::window() {
                    let cb = w.navigator().clipboard();
                    let clone_st = st.send_dc.clone();
                    let clone_ws = st.send_ws.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(val) = wasm_bindgen_futures::JsFuture::from(cb.read_text()).await
                        {
                            let text: String = val.as_string().unwrap_or_default();
                            if !text.is_empty() {
                                let msg = Message::PasteText(text);
                                if let Ok(data) = bincode::serialize(&msg) {
                                    if let Some(ref dc) = clone_st {
                                        let _ = dc.send_with_u8_array(&data);
                                    } else if let Some(ref ws) = clone_ws {
                                        let _ = ws.send_with_u8_array(&data);
                                    }
                                }
                            }
                        }
                    });
                    return;
                }
            }
            // F11: toggle browser fullscreen
            if pressed && code == "F11" {
                if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                    let is_fullscreen = doc.fullscreen_element().is_some();
                    if is_fullscreen {
                        doc.exit_fullscreen();
                    } else if let Some(el) = doc.document_element() {
                        let _ = el.request_fullscreen();
                    }
                }
                return;
            }
            // Cmd/Meta key remapping (macOS → Linux/Windows):
            // Cmd+C/X/Z/A/S → Ctrl+C/X/Z/A/S (remap Cmd to Ctrl)
            // Cmd+R/T/W/Q/L → block (browser shortcuts, page will navigate away)
            if e.meta_key() {
                match code.as_str() {
                    // Browser shortcuts — block entirely (keyup will never arrive)
                    "KeyR" | "KeyT" | "KeyW" | "KeyQ" | "KeyL" | "KeyN" => return,
                    // Remap Cmd+key → Ctrl+key for the remote system
                    _ => {
                        if let Some(kc) = js_code_to_keycode(&code) {
                            if pressed {
                                // Send Ctrl down, key down, key up, Ctrl up
                                send_input(
                                    &st,
                                    InputEvent::Key {
                                        key: KeyCode::LeftCtrl,
                                        pressed: true,
                                    },
                                );
                                send_input(
                                    &st,
                                    InputEvent::Key {
                                        key: kc,
                                        pressed: true,
                                    },
                                );
                                send_input(
                                    &st,
                                    InputEvent::Key {
                                        key: kc,
                                        pressed: false,
                                    },
                                );
                                send_input(
                                    &st,
                                    InputEvent::Key {
                                        key: KeyCode::LeftCtrl,
                                        pressed: false,
                                    },
                                );
                            }
                        }
                        return;
                    }
                }
            }
            if let Some(kc) = js_code_to_keycode(&code) {
                send_input(&st, InputEvent::Key { key: kc, pressed });
            }
        });
        document
            .add_event_listener_with_callback(name, cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }

    // Release all keys on page unload (prevents stuck keys on refresh/navigate)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let st = s.borrow();
            // Release all common modifier and letter keys
            for kc in [
                KeyCode::LeftShift,
                KeyCode::RightShift,
                KeyCode::LeftCtrl,
                KeyCode::RightCtrl,
                KeyCode::LeftAlt,
                KeyCode::RightAlt,
            ] {
                send_input(
                    &st,
                    InputEvent::Key {
                        key: kc,
                        pressed: false,
                    },
                );
            }
        });
        let window = web_sys::window().unwrap();
        let _ =
            window.add_event_listener_with_callback("beforeunload", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Release all keys on tab focus loss (prevents stuck keys on Alt+Tab etc.)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let st = s.borrow();
            for kc in [
                KeyCode::LeftShift,
                KeyCode::RightShift,
                KeyCode::LeftCtrl,
                KeyCode::RightCtrl,
                KeyCode::LeftAlt,
                KeyCode::RightAlt,
            ] {
                send_input(
                    &st,
                    InputEvent::Key {
                        key: kc,
                        pressed: false,
                    },
                );
            }
        });
        let window = web_sys::window().unwrap();
        let _ = window.add_event_listener_with_callback("blur", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // ── Drag & drop file transfer ──────────────────────────────────────
    // Prevent default on dragover (required to allow drop)
    {
        let cb = Closure::<dyn FnMut(web_sys::DragEvent)>::new(move |e: web_sys::DragEvent| {
            e.prevent_default();
            e.stop_propagation();
        });
        let _ = canvas.add_event_listener_with_callback("dragover", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Handle file drop
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(web_sys::DragEvent)>::new(move |e: web_sys::DragEvent| {
            e.prevent_default();
            e.stop_propagation();

            let data_transfer = match e.data_transfer() {
                Some(dt) => dt,
                None => return,
            };
            let files = match data_transfer.files() {
                Some(f) => f,
                None => return,
            };

            for i in 0..files.length() {
                let file = match files.get(i) {
                    Some(f) => f,
                    None => continue,
                };

                let name = file.name();
                let size = file.size() as u64;
                // Generate a transfer ID from random
                let transfer_id = (js_sys::Math::random() * u32::MAX as f64) as u64;

                let state_clone = s.clone();
                let file_name = name.clone();

                console::log_1(&format!("file drop: sending {} ({} bytes)", name, size).into());

                // Send FileOffer
                {
                    let st = s.borrow();
                    let offer = Message::FileOffer {
                        transfer_id,
                        name,
                        size,
                    };
                    send_message(&st, &offer);
                }

                // Read file contents and send chunks asynchronously
                wasm_bindgen_futures::spawn_local(async move {
                    let array_buf =
                        match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                            Ok(ab) => ab,
                            Err(e) => {
                                console::log_1(&format!("file read error: {:?}", e).into());
                                return;
                            }
                        };
                    let data = js_sys::Uint8Array::new(&array_buf).to_vec();

                    // SHA-256 via Web Crypto API
                    let sha256 = match web_crypto_sha256(&data).await {
                        Ok(h) => h,
                        Err(e) => {
                            console::log_1(&format!("SHA-256 error: {:?}", e).into());
                            return;
                        }
                    };

                    // Send in 256KB chunks
                    let chunk_size = 256 * 1024;
                    let total = data.len();
                    let mut offset = 0u64;
                    while (offset as usize) < total {
                        let end = ((offset as usize) + chunk_size).min(total);
                        let chunk_data = data[offset as usize..end].to_vec();
                        let msg = Message::FileChunk {
                            transfer_id,
                            offset,
                            data: chunk_data,
                        };
                        {
                            let st = state_clone.borrow();
                            send_message(&st, &msg);
                        }
                        offset = end as u64;

                        // Log progress every ~1MB
                        if offset as usize == total || (offset % (1024 * 1024)) < chunk_size as u64
                        {
                            let pct = (offset as f64 / total as f64 * 100.0) as u32;
                            console::log_1(&format!("sending {}: {}%", file_name, pct).into());
                        }
                    }

                    // Send FileDone
                    let msg = Message::FileDone {
                        transfer_id,
                        sha256,
                    };
                    {
                        let st = state_clone.borrow();
                        send_message(&st, &msg);
                    }
                    console::log_1(&format!("file sent: {} ({} bytes)", file_name, total).into());
                });
            }
        });
        let _ = canvas.add_event_listener_with_callback("drop", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Window resize → send new resolution to server (debounced 500ms)
    {
        let s = state.clone();
        let timeout_id: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));
        let timeout_id2 = timeout_id.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            let window = web_sys::window().unwrap();
            // Cancel previous timer (debounce)
            if let Some(id) = timeout_id2.borrow_mut().take() {
                window.clear_timeout_with_handle(id);
            }
            let s2 = s.clone();
            let fire = Closure::once_into_js(move || {
                send_resolution_change(&s2);
            });
            let id = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    fire.as_ref().unchecked_ref(),
                    500,
                )
                .unwrap_or(0);
            *timeout_id2.borrow_mut() = Some(id);
        });
        let window = web_sys::window().unwrap();
        let _ =
            window.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

fn send_input(state: &AppState, event: InputEvent) {
    let msg = Message::Input(event);
    send_message(state, &msg);
}

/// Standard resolutions supported by VDD (must match vdd_settings.xml).
const STANDARD_RESOLUTIONS: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1024, 768),
    (1152, 864),
    (1280, 720),
    (1280, 800),
    (1280, 960),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    // Max 1920x1080 — H.264 Baseline Level 4.0 (avc1.42c028) limit.
    // Higher resolutions need Level 5.1 codec string support.
];

/// Find the closest standard resolution that fits within the given viewport.
fn closest_resolution(vw: u32, vh: u32) -> (u32, u32) {
    // Pick the largest standard resolution that fits in the viewport
    let mut best = (640, 480);
    for &(w, h) in STANDARD_RESOLUTIONS {
        if w <= vw && h <= vh {
            best = (w, h);
        }
    }
    best
}

/// Send a ResolutionChange message matching the browser viewport size.
/// Server adjusts VDD virtual display to match (adaptive resolution like DCV/Sunshine).
fn send_resolution_change(state: &Rc<RefCell<AppState>>) {
    let window = web_sys::window().unwrap();
    // Use devicePixelRatio for HiDPI displays
    let dpr = window.device_pixel_ratio().max(1.0);
    let vw = (window.inner_width().unwrap().as_f64().unwrap() * dpr) as u32;
    let vh = (window.inner_height().unwrap().as_f64().unwrap() * dpr) as u32;
    let (w, h) = closest_resolution(vw, vh);
    let st = state.borrow();
    if st.server_width == w && st.server_height == h {
        return; // Already at this resolution
    }
    console::log_1(&format!("Requesting resolution: {w}x{h} (viewport {vw}x{vh})").into());
    let msg = Message::ResolutionChange { width: w, height: h };
    send_message(&st, &msg);
}

fn send_message(state: &AppState, msg: &Message) {
    if let Ok(data) = bincode::serialize(msg) {
        // Prefer DataChannel, fallback to WebSocket
        if let Some(ref dc) = state.send_dc {
            if dc.ready_state() == web_sys::RtcDataChannelState::Open {
                let _ = dc.send_with_u8_array(&data);
                return;
            }
        }
        if let Some(ref ws) = state.send_ws {
            let _ = ws.send_with_u8_array(&data);
        }
    }
}

fn map_mouse(canvas: &HtmlCanvasElement, cx: f64, cy: f64, sw: u32, sh: u32) -> (i32, i32) {
    let rect = canvas.get_bounding_client_rect();
    let aspect = sw as f64 / sh as f64;
    let css_aspect = rect.width() / rect.height();
    let (rw, rh, ox, oy) = if aspect > css_aspect {
        (
            rect.width(),
            rect.width() / aspect,
            0.0,
            (rect.height() - rect.width() / aspect) / 2.0,
        )
    } else {
        (
            rect.height() * aspect,
            rect.height(),
            (rect.width() - rect.height() * aspect) / 2.0,
            0.0,
        )
    };
    let x = ((cx - rect.left() - ox) / rw * sw as f64).clamp(0.0, sw as f64 - 1.0) as i32;
    let y = ((cy - rect.top() - oy) / rh * sh as f64).clamp(0.0, sh as f64 - 1.0) as i32;
    (x, y)
}

fn js_code_to_keycode(code: &str) -> Option<KeyCode> {
    Some(match code {
        "KeyA" => KeyCode::A,
        "KeyB" => KeyCode::B,
        "KeyC" => KeyCode::C,
        "KeyD" => KeyCode::D,
        "KeyE" => KeyCode::E,
        "KeyF" => KeyCode::F,
        "KeyG" => KeyCode::G,
        "KeyH" => KeyCode::H,
        "KeyI" => KeyCode::I,
        "KeyJ" => KeyCode::J,
        "KeyK" => KeyCode::K,
        "KeyL" => KeyCode::L,
        "KeyM" => KeyCode::M,
        "KeyN" => KeyCode::N,
        "KeyO" => KeyCode::O,
        "KeyP" => KeyCode::P,
        "KeyQ" => KeyCode::Q,
        "KeyR" => KeyCode::R,
        "KeyS" => KeyCode::S,
        "KeyT" => KeyCode::T,
        "KeyU" => KeyCode::U,
        "KeyV" => KeyCode::V,
        "KeyW" => KeyCode::W,
        "KeyX" => KeyCode::X,
        "KeyY" => KeyCode::Y,
        "KeyZ" => KeyCode::Z,
        "Digit0" => KeyCode::Key0,
        "Digit1" => KeyCode::Key1,
        "Digit2" => KeyCode::Key2,
        "Digit3" => KeyCode::Key3,
        "Digit4" => KeyCode::Key4,
        "Digit5" => KeyCode::Key5,
        "Digit6" => KeyCode::Key6,
        "Digit7" => KeyCode::Key7,
        "Digit8" => KeyCode::Key8,
        "Digit9" => KeyCode::Key9,
        "F1" => KeyCode::F1,
        "F2" => KeyCode::F2,
        "F3" => KeyCode::F3,
        "F4" => KeyCode::F4,
        "F5" => KeyCode::F5,
        "F6" => KeyCode::F6,
        "F7" => KeyCode::F7,
        "F8" => KeyCode::F8,
        "F9" => KeyCode::F9,
        "F10" => KeyCode::F10,
        "F11" => KeyCode::F11,
        "F12" => KeyCode::F12,
        "ShiftLeft" => KeyCode::LeftShift,
        "ShiftRight" => KeyCode::RightShift,
        "ControlLeft" => KeyCode::LeftCtrl,
        "ControlRight" => KeyCode::RightCtrl,
        "AltLeft" => KeyCode::LeftAlt,
        "AltRight" => KeyCode::RightAlt,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Tab" => KeyCode::Tab,
        "Enter" => KeyCode::Enter,
        "Space" => KeyCode::Space,
        "Escape" => KeyCode::Escape,
        "Insert" => KeyCode::Insert,
        "Minus" => KeyCode::Minus,
        "Equal" => KeyCode::Equal,
        "BracketLeft" => KeyCode::LeftBracket,
        "BracketRight" => KeyCode::RightBracket,
        "Backslash" => KeyCode::Backslash,
        "Semicolon" => KeyCode::Semicolon,
        "Quote" => KeyCode::Apostrophe,
        "Backquote" => KeyCode::Grave,
        "Comma" => KeyCode::Comma,
        "Period" => KeyCode::Period,
        "Slash" => KeyCode::Slash,
        "CapsLock" => KeyCode::CapsLock,
        "NumLock" => KeyCode::NumLock,
        "ScrollLock" => KeyCode::ScrollLock,
        _ => return None,
    })
}

/// Compute SHA-256 hash using Web Crypto API (SubtleCrypto).
async fn web_crypto_sha256(data: &[u8]) -> Result<[u8; 32], JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let crypto = window
        .crypto()
        .map_err(|_| JsValue::from_str("no crypto"))?;
    let subtle = crypto.subtle();

    let buf = js_sys::Uint8Array::from(data);
    let promise = subtle.digest_with_str_and_buffer_source("SHA-256", &buf)?;
    let result = wasm_bindgen_futures::JsFuture::from(promise).await?;
    let array = js_sys::Uint8Array::new(&result);
    let mut hash = [0u8; 32];
    array.copy_to(&mut hash);
    Ok(hash)
}
