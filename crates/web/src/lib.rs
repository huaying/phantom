use phantom_core::encode::VideoCodec;
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
use phantom_core::protocol::Message;
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

// -- Shared state --

struct AppState {
    ws: WebSocket,
    ctx: CanvasRenderingContext2d,
    canvas: HtmlCanvasElement,
    decoder: Option<JsVideoDecoder>,
    server_width: u32,
    server_height: u32,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

// -- Entry point --

#[wasm_bindgen(start)]
pub fn main() {
    console::log_1(&"Phantom Web Client starting...".into());

    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    let host = window.location().host().unwrap();

    // Create fullscreen canvas
    let canvas: HtmlCanvasElement = document.create_element("canvas").unwrap()
        .dyn_into().unwrap();
    canvas.set_id("screen");
    let style = canvas.style();
    style.set_property("width", "100vw").unwrap();
    style.set_property("height", "100vh").unwrap();
    style.set_property("display", "block").unwrap();
    style.set_property("cursor", "none").unwrap();

    let body = document.body().unwrap();
    let body_style = body.style();
    body_style.set_property("margin", "0").unwrap();
    body_style.set_property("overflow", "hidden").unwrap();
    body_style.set_property("background", "#000").unwrap();
    body.append_child(&canvas).unwrap();

    let ctx: CanvasRenderingContext2d = canvas.get_context("2d").unwrap().unwrap().dyn_into().unwrap();
    ctx.set_font("20px monospace");
    ctx.set_fill_style_str("white");
    let _ = ctx.fill_text("Connecting...", 20.0, 40.0);

    // WebSocket
    let ws = WebSocket::new(&format!("ws://{host}/ws")).expect("WebSocket failed");
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let state = Rc::new(RefCell::new(AppState {
        ws: ws.clone(),
        ctx,
        canvas: canvas.clone(),
        decoder: None,
        server_width: 0,
        server_height: 0,
    }));

    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // WS callbacks
    {
        let onopen = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"Connected".into());
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();
    }
    {
        let s = state.clone();
        let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let data = js_sys::Uint8Array::new(&buf).to_vec();
                on_message(&s, &data);
            }
        });
        ws.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
        onmsg.forget();
    }
    {
        let onclose = Closure::<dyn FnMut()>::new(|| {
            console::log_1(&"Disconnected, reloading...".into());
            let w = web_sys::window().unwrap();
            let cb = Closure::<dyn FnMut()>::new(|| {
                let _ = web_sys::window().unwrap().location().reload();
            });
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 2000);
            cb.forget();
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();
    }

    // Input
    setup_input(&canvas, &document, &state);
}

fn on_message(state: &Rc<RefCell<AppState>>, data: &[u8]) {
    // Server sends: [4-byte len][bincode payload] (same as TCP framing)
    // But WebSocket already handles framing, so the server should send raw bincode.
    // We try both: first raw bincode, then length-prefixed.
    let msg: Message = match bincode::deserialize(data) {
        Ok(m) => m,
        Err(_) => {
            // Try skipping 4-byte length prefix
            if data.len() > 4 {
                match bincode::deserialize(&data[4..]) {
                    Ok(m) => m,
                    Err(_) => return,
                }
            } else {
                return;
            }
        }
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
        Message::VideoFrame { frame, .. } => {
            if frame.codec != VideoCodec::H264 || frame.data.is_empty() { return; }
            let s = state.borrow();
            if let Some(ref decoder) = s.decoder {
                let data_js = js_sys::Uint8Array::from(frame.data.as_slice());
                let init = js_sys::Object::new();
                js_sys::Reflect::set(&init, &"type".into(),
                    &if frame.is_keyframe { "key" } else { "delta" }.into()).unwrap();
                js_sys::Reflect::set(&init, &"timestamp".into(), &(0.0_f64).into()).unwrap();
                js_sys::Reflect::set(&init, &"data".into(), &data_js.buffer()).unwrap();
                let chunk = JsEncodedVideoChunk::new(&init);
                decoder.decode(&chunk);
            }
        }
        Message::ClipboardSync(text) => {
            if let Some(w) = web_sys::window() {
                let nav = w.navigator();
                {
                    let cb = nav.clipboard();
                    let _ = cb.write_text(&text);
                }
            }
        }
        _ => {}
    }
}

fn setup_decoder(state: &Rc<RefCell<AppState>>, width: u32, height: u32) {
    let s = state.clone();

    let output_cb = Closure::<dyn FnMut(JsValue)>::new(move |frame: JsValue| {
        let st = s.borrow();
        // Use raw JS: ctx.drawImage(videoFrame, 0, 0, w, h)
        let args = js_sys::Array::new();
        args.push(&frame);
        args.push(&0.into());
        args.push(&0.into());
        args.push(&(st.canvas.width() as f64).into());
        args.push(&(st.canvas.height() as f64).into());
        let draw_fn = js_sys::Reflect::get(&st.ctx, &"drawImage".into()).unwrap();
        let _ = js_sys::Reflect::apply(&draw_fn.into(), &st.ctx, &args);
        // frame.close()
        let close_fn = js_sys::Reflect::get(&frame, &"close".into()).unwrap();
        let _ = js_sys::Reflect::apply(&close_fn.into(), &frame, &js_sys::Array::new());
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

fn setup_input(canvas: &HtmlCanvasElement, document: &web_sys::Document, state: &Rc<RefCell<AppState>>) {
    // Mouse move
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            let st = s.borrow();
            if st.server_width == 0 { return; }
            let rect = st.canvas.get_bounding_client_rect();
            let sx = st.server_width as f64 / rect.width();
            let sy = st.server_height as f64 / rect.height();
            let x = ((e.client_x() as f64 - rect.left()) * sx) as i32;
            let y = ((e.client_y() as f64 - rect.top()) * sy) as i32;
            send_input(&st.ws, InputEvent::MouseMove { x, y });
        });
        canvas.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Mouse buttons
    for name in &["mousedown", "mouseup"] {
        let s = state.clone();
        let pressed = *name == "mousedown";
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.prevent_default();
            let st = s.borrow();
            let btn = match e.button() {
                0 => MouseButton::Left, 1 => MouseButton::Middle,
                2 => MouseButton::Right, _ => return,
            };
            send_input(&st.ws, InputEvent::MouseButton { button: btn, pressed });
        });
        canvas.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Right-click menu prevention
    {
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(|e: MouseEvent| e.prevent_default());
        canvas.add_event_listener_with_callback("contextmenu", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Scroll
    {
        let s = state.clone();
        let cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
            e.prevent_default();
            let st = s.borrow();
            send_input(&st.ws, InputEvent::MouseScroll {
                dx: e.delta_x() as f32 / 120.0,
                dy: e.delta_y() as f32 / 120.0,
            });
        });
        canvas.add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Keyboard
    for name in &["keydown", "keyup"] {
        let s = state.clone();
        let pressed = *name == "keydown";
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            e.prevent_default();
            let st = s.borrow();

            // Paste interception
            if pressed && e.code() == "KeyV" && (e.ctrl_key() || e.meta_key()) {
                let w: Option<web_sys::Window> = web_sys::window();
                if let Some(w) = w {
                    let nav = w.navigator();
                    {
                    let cb = nav.clipboard();
                        let ws = st.ws.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let promise = cb.read_text();
                            match wasm_bindgen_futures::JsFuture::from(promise).await {
                                Ok(val) => {
                                    let text: String = val.as_string().unwrap_or_default();
                                    if !text.is_empty() {
                                        send_msg(&ws, &Message::PasteText(text));
                                    }
                                }
                                Err(_) => {}
                            }
                        });
                        return;
                    }
                }
            }

            if let Some(kc) = js_code_to_keycode(&e.code()) {
                send_input(&st.ws, InputEvent::Key { key: kc, pressed });
            }
        });
        document.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
}

fn send_input(ws: &WebSocket, event: InputEvent) {
    send_msg(ws, &Message::Input(event));
}

fn send_msg(ws: &WebSocket, msg: &Message) {
    if let Ok(data) = bincode::serialize(msg) {
        let _ = ws.send_with_u8_array(&data);
    }
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
        "MetaLeft" => KeyCode::LeftMeta, "MetaRight" => KeyCode::RightMeta,
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
