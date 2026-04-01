use phantom_core::encode::{TileEncoding, VideoCodec};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
use phantom_core::protocol::Message;
use phantom_core::tile::TILE_SIZE;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    console, CanvasRenderingContext2d, HtmlCanvasElement, KeyboardEvent, MessageEvent, MouseEvent,
    WebSocket, WheelEvent,
};
use std::cell::RefCell;
use std::rc::Rc;

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
}

struct AppState {
    ctx: CanvasRenderingContext2d,
    canvas: HtmlCanvasElement,
    decoder: Option<JsVideoDecoder>,
    server_width: u32,
    server_height: u32,
    frame_count: u64,
    /// Whether we've received at least one keyframe. WebCodecs requires a
    /// keyframe before it can decode any delta frames — skip deltas until then.
    got_keyframe: bool,
    /// Highest sequence number from a fully rendered VideoFrame.
    /// TileUpdates with sequence <= this are stale and should be skipped.
    last_video_sequence: u64,
    /// For sending input — either DataChannel or WebSocket
    send_dc: Option<web_sys::RtcDataChannel>,
    send_ws: Option<WebSocket>,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

#[wasm_bindgen(start)]
pub fn main() {
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();

    // Check URL for ?ws to use WebSocket mode
    let use_ws = window.location().search().unwrap_or_default().contains("ws");
    let mode = if use_ws { "WebSocket" } else { "WebRTC" };
    console::log_1(&format!("Phantom Web Client starting ({mode} mode)...").into());

    let canvas: HtmlCanvasElement = document.get_element_by_id("screen").unwrap()
        .dyn_into().unwrap();
    let ctx: CanvasRenderingContext2d = canvas.get_context("2d").unwrap().unwrap().dyn_into().unwrap();

    let state = Rc::new(RefCell::new(AppState {
        ctx,
        canvas: canvas.clone(),
        decoder: None,
        server_width: 0,
        server_height: 0,
        frame_count: 0,
        got_keyframe: false,
        last_video_sequence: 0,
        send_dc: None,
        send_ws: None,
    }));

    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // Setup input listeners on canvas
    setup_input(&canvas, &document, &state);

    if use_ws {
        setup_ws(&state);
    } else {
        setup_webrtc(&state);
    }
}

fn setup_webrtc(state: &Rc<RefCell<AppState>>) {
    use web_sys::{RtcConfiguration, RtcDataChannelInit, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit};

    let config = RtcConfiguration::new();
    let pc = match RtcPeerConnection::new_with_configuration(&config) {
        Ok(pc) => pc,
        Err(e) => {
            console::error_1(&format!("WebRTC not available: {:?}", e).into());
            return;
        }
    };

    // Create 3 DataChannels
    // Video DC: ordered + reliable for now (ensures keyframes arrive intact).
    // TODO: switch to unreliable + periodic keyframes for lower latency.
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

    // onmessage for video DC (receives VideoFrame/TileUpdate)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                on_message(&s, &js_sys::Uint8Array::new(&buf).to_vec());
            }
        });
        video_dc.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onmessage for control DC (receives Hello, Clipboard, etc.)
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                on_message(&s, &js_sys::Uint8Array::new(&buf).to_vec());
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
            Err(e) => { console::error_1(&format!("createOffer: {:?}", e).into()); return; }
        };
        let sdp = js_sys::Reflect::get(&offer, &"sdp".into()).unwrap();
        let sdp_str: String = sdp.as_string().unwrap_or_default();

        // Set local description
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        desc.set_sdp(&sdp_str);
        if let Err(e) = wasm_bindgen_futures::JsFuture::from(pc2.set_local_description(&desc)).await {
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
            window.fetch_with_str_and_init("/rtc", &request_init)
        ).await {
            Ok(r) => r,
            Err(e) => { console::error_1(&format!("POST /rtc failed: {:?}", e).into()); return; }
        };

        let resp: web_sys::Response = resp.dyn_into().unwrap();
        if !resp.ok() {
            console::error_1(&format!("POST /rtc status: {}", resp.status()).into());
            return;
        }

        let json = match wasm_bindgen_futures::JsFuture::from(resp.json().unwrap()).await {
            Ok(j) => j,
            Err(e) => { console::error_1(&format!("parse answer: {:?}", e).into()); return; }
        };

        // Set remote description (answer)
        let answer_sdp = js_sys::Reflect::get(&json, &"sdp".into()).unwrap();
        let answer_desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        answer_desc.set_sdp(&answer_sdp.as_string().unwrap_or_default());

        if let Err(e) = wasm_bindgen_futures::JsFuture::from(pc2.set_remote_description(&answer_desc)).await {
            console::error_1(&format!("setRemoteDescription: {:?}", e).into());
            return;
        }

        console::log_1(&"WebRTC: SDP exchange complete, waiting for ICE...".into());
    });

    // Keep PC alive
    js_sys::Reflect::set(&js_sys::global(), &"__phantom_pc".into(), &pc).unwrap();
}

fn setup_ws(state: &Rc<RefCell<AppState>>) {
    let window = web_sys::window().unwrap();
    let location = window.location();
    let host = location.host().unwrap_or_default(); // includes port
    // Use wss:// on same port as HTTPS (same self-signed cert, no mixed content)
    let protocol = if location.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
    let ws_url = format!("{protocol}://{host}/ws");
    console::log_1(&format!("Connecting to {ws_url}...").into());

    let ws = match WebSocket::new(&ws_url) {
        Ok(ws) => ws,
        Err(e) => { console::error_1(&format!("WebSocket error: {:?}", e).into()); return; }
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

    // onopen
    {
        let cb = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"WebSocket connected!".into());
        });
        ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    // onerror
    {
        let cb = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(|e: web_sys::ErrorEvent| {
            console::error_1(&format!("WebSocket error: {:?}", e.message()).into());
        });
        ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    state.borrow_mut().send_ws = Some(ws);
}

// -- Message handling (same for WebRTC and WS) --

fn on_message(state: &Rc<RefCell<AppState>>, data: &[u8]) {
    let msg: Message = match bincode::deserialize(data) {
        Ok(m) => m,
        Err(_) => return,
    };

    match msg {
        Message::Hello { width, height, .. } => {
            console::log_1(&format!("Server: {width}x{height}").into());
            let mut s = state.borrow_mut();
            s.server_width = width;
            s.server_height = height;
            s.canvas.set_width(width);
            s.canvas.set_height(height);
            drop(s);
            setup_decoder(state, width, height);
        }
        Message::VideoFrame { sequence, frame } => {
            if frame.codec != VideoCodec::H264 || frame.data.is_empty() { return; }
            let mut s = state.borrow_mut();
            let is_key = h264_has_idr(&frame.data);
            // Log first few frames for debugging
            if s.frame_count < 5 {
                console::log_1(&format!("frame #{}: {} bytes, encoder_kf={}, nal_idr={}", s.frame_count, frame.data.len(), frame.is_keyframe, is_key).into());
            }
            // WebCodecs requires a keyframe before any delta frames can be decoded.
            // Skip delta frames until we receive the first keyframe.
            if !s.got_keyframe && !is_key {
                console::warn_1(&format!("skipping frame #{} (waiting for keyframe)", s.frame_count).into());
                s.frame_count += 1;
                return;
            }
            if is_key { s.got_keyframe = true; }
            s.frame_count += 1;
            s.last_video_sequence = sequence;
            let fc = s.frame_count;
            if let Some(ref decoder) = s.decoder {
                let data_js = js_sys::Uint8Array::from(frame.data.as_slice());
                let init = js_sys::Object::new();
                js_sys::Reflect::set(&init, &"type".into(),
                    &if is_key { "key" } else { "delta" }.into()).unwrap();
                js_sys::Reflect::set(&init, &"timestamp".into(),
                    &(fc as f64 * 33333.0).into()).unwrap();
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
                            Ok(d) => d, Err(_) => continue,
                        };
                        let mut out = Vec::new();
                        if std::io::Read::read_to_end(&mut dec, &mut out).is_err() { continue; }
                        out
                    }
                    TileEncoding::Raw => tile.data.clone(),
                    _ => continue,
                };
                let tw = tile.pixel_width as usize;
                let th = tile.pixel_height as usize;
                if bgra.len() < tw * th * 4 { continue; }
                let mut rgba = vec![0u8; tw * th * 4];
                for i in 0..tw * th {
                    rgba[i*4] = bgra[i*4+2]; rgba[i*4+1] = bgra[i*4+1];
                    rgba[i*4+2] = bgra[i*4]; rgba[i*4+3] = 255;
                }
                let clamped = wasm_bindgen::Clamped(&rgba[..]);
                if let Ok(img) = web_sys::ImageData::new_with_u8_clamped_array_and_sh(clamped, tw as u32, th as u32) {
                    let _ = s.ctx.put_image_data(&img, (tile.tile_x * TILE_SIZE) as f64, (tile.tile_y * TILE_SIZE) as f64);
                }
            }
        }
        Message::ClipboardSync(_text) => {
            // Clipboard write requires document focus and secure context.
            // Silently ignore — don't let it crash the message handler.
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

fn setup_decoder(state: &Rc<RefCell<AppState>>, width: u32, height: u32) {
    let s = state.clone();
    let decode_count = Rc::new(RefCell::new(0u64));
    let dc = decode_count.clone();

    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |frame: JsValue| {
        let mut count = dc.borrow_mut();
        *count += 1;
        if *count <= 3 { console::log_1(&format!("Decoded frame #{}", *count).into()); }
        let st = s.borrow();
        let w = st.canvas.width();
        let h = st.canvas.height();
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

    let config = js_sys::Object::new();
    js_sys::Reflect::set(&config, &"codec".into(), &"avc1.42001f".into()).unwrap();
    js_sys::Reflect::set(&config, &"codedWidth".into(), &(width).into()).unwrap();
    js_sys::Reflect::set(&config, &"codedHeight".into(), &(height).into()).unwrap();
    js_sys::Reflect::set(&config, &"optimizeForLatency".into(), &true.into()).unwrap();
    decoder.configure(&config);

    state.borrow_mut().decoder = Some(decoder);
    console::log_1(&"H.264 decoder ready".into());
    output_cb.forget();
    error_cb.forget();
}

// -- Input --

fn setup_input(canvas: &HtmlCanvasElement, document: &web_sys::Document, state: &Rc<RefCell<AppState>>) {
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            let st = s.borrow();
            if st.server_width == 0 || st.server_height == 0 { return; }
            let (x, y) = map_mouse(&st.canvas, e.client_x() as f64, e.client_y() as f64, st.server_width, st.server_height);
            send_input(&st, InputEvent::MouseMove { x, y });
        });
        canvas.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    for name in &["mousedown", "mouseup"] {
        let s = state.clone();
        let pressed = *name == "mousedown";
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.prevent_default();
            let st = s.borrow();
            let btn = match e.button() { 0 => MouseButton::Left, 1 => MouseButton::Middle, 2 => MouseButton::Right, _ => return };
            send_input(&st, InputEvent::MouseButton { button: btn, pressed });
        });
        canvas.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(|e: MouseEvent| e.prevent_default());
        canvas.add_event_listener_with_callback("contextmenu", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
            e.prevent_default();
            let st = s.borrow();
            send_input(&st, InputEvent::MouseScroll { dx: e.delta_x() as f32 / 120.0, dy: e.delta_y() as f32 / 120.0 });
        });
        canvas.add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    for name in &["keydown", "keyup"] {
        let s = state.clone();
        let pressed = *name == "keydown";
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            e.prevent_default();
            let st = s.borrow();
            let code = e.code();
            if code == "MetaLeft" || code == "MetaRight" { return; }
            // Paste
            if pressed && code == "KeyV" && (e.ctrl_key() || e.meta_key()) {
                if let Some(w) = web_sys::window() {
                    let cb = w.navigator().clipboard();
                    let clone_st = st.send_dc.clone();
                    let clone_ws = st.send_ws.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(val) = wasm_bindgen_futures::JsFuture::from(cb.read_text()).await {
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
            if let Some(kc) = js_code_to_keycode(&code) {
                send_input(&st, InputEvent::Key { key: kc, pressed });
            }
        });
        document.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
}

fn send_input(state: &AppState, event: InputEvent) {
    let msg = Message::Input(event);
    if let Ok(data) = bincode::serialize(&msg) {
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
        (rect.width(), rect.width() / aspect, 0.0, (rect.height() - rect.width() / aspect) / 2.0)
    } else {
        (rect.height() * aspect, rect.height(), (rect.width() - rect.height() * aspect) / 2.0, 0.0)
    };
    let x = ((cx - rect.left() - ox) / rw * sw as f64).clamp(0.0, sw as f64 - 1.0) as i32;
    let y = ((cy - rect.top() - oy) / rh * sh as f64).clamp(0.0, sh as f64 - 1.0) as i32;
    (x, y)
}

fn js_code_to_keycode(code: &str) -> Option<KeyCode> {
    Some(match code {
        "KeyA" => KeyCode::A, "KeyB" => KeyCode::B, "KeyC" => KeyCode::C,
        "KeyD" => KeyCode::D, "KeyE" => KeyCode::E, "KeyF" => KeyCode::F,
        "KeyG" => KeyCode::G, "KeyH" => KeyCode::H, "KeyI" => KeyCode::I,
        "KeyJ" => KeyCode::J, "KeyK" => KeyCode::K, "KeyL" => KeyCode::L,
        "KeyM" => KeyCode::M, "KeyN" => KeyCode::N, "KeyO" => KeyCode::O,
        "KeyP" => KeyCode::P, "KeyQ" => KeyCode::Q, "KeyR" => KeyCode::R,
        "KeyS" => KeyCode::S, "KeyT" => KeyCode::T, "KeyU" => KeyCode::U,
        "KeyV" => KeyCode::V, "KeyW" => KeyCode::W, "KeyX" => KeyCode::X,
        "KeyY" => KeyCode::Y, "KeyZ" => KeyCode::Z,
        "Digit0" => KeyCode::Key0, "Digit1" => KeyCode::Key1,
        "Digit2" => KeyCode::Key2, "Digit3" => KeyCode::Key3,
        "Digit4" => KeyCode::Key4, "Digit5" => KeyCode::Key5,
        "Digit6" => KeyCode::Key6, "Digit7" => KeyCode::Key7,
        "Digit8" => KeyCode::Key8, "Digit9" => KeyCode::Key9,
        "F1" => KeyCode::F1, "F2" => KeyCode::F2, "F3" => KeyCode::F3,
        "F4" => KeyCode::F4, "F5" => KeyCode::F5, "F6" => KeyCode::F6,
        "F7" => KeyCode::F7, "F8" => KeyCode::F8, "F9" => KeyCode::F9,
        "F10" => KeyCode::F10, "F11" => KeyCode::F11, "F12" => KeyCode::F12,
        "ShiftLeft" => KeyCode::LeftShift, "ShiftRight" => KeyCode::RightShift,
        "ControlLeft" => KeyCode::LeftCtrl, "ControlRight" => KeyCode::RightCtrl,
        "AltLeft" => KeyCode::LeftAlt, "AltRight" => KeyCode::RightAlt,
        "ArrowUp" => KeyCode::Up, "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left, "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home, "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp, "PageDown" => KeyCode::PageDown,
        "Backspace" => KeyCode::Backspace, "Delete" => KeyCode::Delete,
        "Tab" => KeyCode::Tab, "Enter" => KeyCode::Enter,
        "Space" => KeyCode::Space, "Escape" => KeyCode::Escape,
        "Insert" => KeyCode::Insert,
        "Minus" => KeyCode::Minus, "Equal" => KeyCode::Equal,
        "BracketLeft" => KeyCode::LeftBracket, "BracketRight" => KeyCode::RightBracket,
        "Backslash" => KeyCode::Backslash, "Semicolon" => KeyCode::Semicolon,
        "Quote" => KeyCode::Apostrophe, "Backquote" => KeyCode::Grave,
        "Comma" => KeyCode::Comma, "Period" => KeyCode::Period,
        "Slash" => KeyCode::Slash, "CapsLock" => KeyCode::CapsLock,
        "NumLock" => KeyCode::NumLock, "ScrollLock" => KeyCode::ScrollLock,
        _ => return None,
    })
}
