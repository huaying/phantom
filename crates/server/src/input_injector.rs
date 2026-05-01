use anyhow::Result;
#[cfg(not(target_os = "windows"))]
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::POINT;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::*;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// Injects input events into the OS.
///
/// Windows uses native `SendInput` so the service agent can avoid enigo's
/// desktop/window initialization path. Linux uses uinput for keyboard when
/// available, falling back to enigo/XTest; other platforms use enigo.
pub struct InputInjector {
    #[cfg(not(target_os = "windows"))]
    enigo: Enigo,
    #[cfg(target_os = "linux")]
    uinput: Option<crate::input_uinput::UinputKeyboard>,
}

impl InputInjector {
    pub fn new() -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            windows_release_modifiers();
            tracing::info!("InputInjector initialized (Windows native SendInput)");
            return Ok(Self {});
        }

        #[cfg(not(target_os = "windows"))]
        {
            let mut enigo = Enigo::new(&Settings::default())
                .map_err(|e| anyhow::anyhow!("failed to init enigo: {e}"))?;

            // Release all modifier keys to clear any stuck state from previous sessions
            for key in [Key::Shift, Key::Control, Key::Alt, Key::Meta] {
                let _ = enigo.key(key, Direction::Release);
            }

            // Try uinput for keyboard on Linux. Non-fatal if it fails — we
            // keep running with enigo/XTest but log loudly so the operator
            // knows the GDM-42 / Wayland / lock-screen scenarios won't work
            // reliably until permissions are fixed.
            #[cfg(target_os = "linux")]
            let uinput = match crate::input_uinput::UinputKeyboard::new() {
                Ok(u) => {
                    tracing::info!("InputInjector: keyboard via uinput");
                    Some(u)
                }
                Err(e) => {
                    tracing::warn!(
                        "uinput keyboard unavailable, falling back to XTest: {e:#}. \
                     Run install.sh to add a udev rule + the 'input' group so \
                     login-screen typing works reliably."
                    );
                    None
                }
            };

            tracing::info!("InputInjector initialized");
            Ok(Self {
                enigo,
                #[cfg(target_os = "linux")]
                uinput,
            })
        }
    }

    /// Type out a string (for paste operations).
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            windows_type_text(text)
        }

        #[cfg(not(target_os = "windows"))]
        {
            self.enigo
                .text(text)
                .map_err(|e| anyhow::anyhow!("type text: {e}"))?;
            Ok(())
        }
    }

    pub fn inject(&mut self, event: &InputEvent) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            windows_inject(event)
        }

        #[cfg(not(target_os = "windows"))]
        {
            self.inject_enigo(event)
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn inject_enigo(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y } => {
                tracing::trace!(x, y, "mouse move");
                self.enigo
                    .move_mouse(*x, *y, Coordinate::Abs)
                    .map_err(|e| anyhow::anyhow!("mouse move: {e}"))?;
            }
            InputEvent::MouseButton { button, pressed } => {
                let btn = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Right => Button::Right,
                    MouseButton::Middle => Button::Middle,
                };
                let dir = if *pressed {
                    Direction::Press
                } else {
                    Direction::Release
                };
                self.enigo
                    .button(btn, dir)
                    .map_err(|e| anyhow::anyhow!("mouse button: {e}"))?;
            }
            InputEvent::MouseScroll { dx, dy } => {
                // dx/dy are line counts (1.0 = one scroll notch).
                // enigo scroll(N) sends N button clicks (ScrollUp/Down/Left/Right).
                if *dy != 0.0 {
                    self.enigo
                        .scroll(*dy as i32, enigo::Axis::Vertical)
                        .map_err(|e| anyhow::anyhow!("scroll: {e}"))?;
                }
                if *dx != 0.0 {
                    self.enigo
                        .scroll(*dx as i32, enigo::Axis::Horizontal)
                        .map_err(|e| anyhow::anyhow!("scroll: {e}"))?;
                }
            }
            InputEvent::Key { key, pressed } => {
                #[cfg(target_os = "linux")]
                if let Some(ref mut u) = self.uinput {
                    tracing::trace!(?key, pressed, "key inject (uinput)");
                    match u.inject_key(*key, *pressed) {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            // Device died unexpectedly (e.g. module unloaded).
                            // Drop to enigo path for this call and future ones.
                            tracing::warn!("uinput inject failed: {e:#}; dropping uinput backend");
                            self.uinput = None;
                        }
                    }
                }
                if let Some(enigo_key) = keycode_to_enigo(*key) {
                    let dir = if *pressed {
                        Direction::Press
                    } else {
                        Direction::Release
                    };
                    tracing::trace!(?key, ?enigo_key, ?dir, "key inject (enigo)");
                    self.enigo
                        .key(enigo_key, dir)
                        .map_err(|e| anyhow::anyhow!("key: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn keycode_to_enigo(key: KeyCode) -> Option<Key> {
    Some(match key {
        // Letters → Unicode
        KeyCode::A => Key::Unicode('a'),
        KeyCode::B => Key::Unicode('b'),
        KeyCode::C => Key::Unicode('c'),
        KeyCode::D => Key::Unicode('d'),
        KeyCode::E => Key::Unicode('e'),
        KeyCode::F => Key::Unicode('f'),
        KeyCode::G => Key::Unicode('g'),
        KeyCode::H => Key::Unicode('h'),
        KeyCode::I => Key::Unicode('i'),
        KeyCode::J => Key::Unicode('j'),
        KeyCode::K => Key::Unicode('k'),
        KeyCode::L => Key::Unicode('l'),
        KeyCode::M => Key::Unicode('m'),
        KeyCode::N => Key::Unicode('n'),
        KeyCode::O => Key::Unicode('o'),
        KeyCode::P => Key::Unicode('p'),
        KeyCode::Q => Key::Unicode('q'),
        KeyCode::R => Key::Unicode('r'),
        KeyCode::S => Key::Unicode('s'),
        KeyCode::T => Key::Unicode('t'),
        KeyCode::U => Key::Unicode('u'),
        KeyCode::V => Key::Unicode('v'),
        KeyCode::W => Key::Unicode('w'),
        KeyCode::X => Key::Unicode('x'),
        KeyCode::Y => Key::Unicode('y'),
        KeyCode::Z => Key::Unicode('z'),

        // Numbers
        KeyCode::Key0 => Key::Unicode('0'),
        KeyCode::Key1 => Key::Unicode('1'),
        KeyCode::Key2 => Key::Unicode('2'),
        KeyCode::Key3 => Key::Unicode('3'),
        KeyCode::Key4 => Key::Unicode('4'),
        KeyCode::Key5 => Key::Unicode('5'),
        KeyCode::Key6 => Key::Unicode('6'),
        KeyCode::Key7 => Key::Unicode('7'),
        KeyCode::Key8 => Key::Unicode('8'),
        KeyCode::Key9 => Key::Unicode('9'),

        // Function keys
        KeyCode::F1 => Key::F1,
        KeyCode::F2 => Key::F2,
        KeyCode::F3 => Key::F3,
        KeyCode::F4 => Key::F4,
        KeyCode::F5 => Key::F5,
        KeyCode::F6 => Key::F6,
        KeyCode::F7 => Key::F7,
        KeyCode::F8 => Key::F8,
        KeyCode::F9 => Key::F9,
        KeyCode::F10 => Key::F10,
        KeyCode::F11 => Key::F11,
        KeyCode::F12 => Key::F12,

        // Modifiers
        KeyCode::LeftShift | KeyCode::RightShift => Key::Shift,
        KeyCode::LeftCtrl | KeyCode::RightCtrl => Key::Control,
        KeyCode::LeftAlt | KeyCode::RightAlt => Key::Alt,
        KeyCode::LeftMeta | KeyCode::RightMeta => Key::Meta,

        // Navigation
        KeyCode::Up => Key::UpArrow,
        KeyCode::Down => Key::DownArrow,
        KeyCode::Left => Key::LeftArrow,
        KeyCode::Right => Key::RightArrow,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,

        // Editing
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab,
        KeyCode::Enter => Key::Return,
        KeyCode::Space => Key::Space,
        KeyCode::Escape => Key::Escape,

        // Punctuation → Unicode
        KeyCode::Minus => Key::Unicode('-'),
        KeyCode::Equal => Key::Unicode('='),
        KeyCode::LeftBracket => Key::Unicode('['),
        KeyCode::RightBracket => Key::Unicode(']'),
        KeyCode::Backslash => Key::Unicode('\\'),
        KeyCode::Semicolon => Key::Unicode(';'),
        KeyCode::Apostrophe => Key::Unicode('\''),
        KeyCode::Grave => Key::Unicode('`'),
        KeyCode::Comma => Key::Unicode(','),
        KeyCode::Period => Key::Unicode('.'),
        KeyCode::Slash => Key::Unicode('/'),

        KeyCode::CapsLock => Key::CapsLock,
        _ => return None,
    })
}

#[cfg(target_os = "windows")]
fn windows_inject(event: &InputEvent) -> Result<()> {
    match event {
        InputEvent::MouseMove { x, y } => windows_mouse_move(*x, *y),
        InputEvent::MouseButton { button, pressed } => {
            let flags = match (button, pressed) {
                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            windows_send_input(&[mouse_input(0, 0, 0, flags)])
        }
        InputEvent::MouseScroll { dx, dy } => {
            const WHEEL_DELTA: i32 = 120;
            let mut inputs = Vec::with_capacity(2);
            if *dy != 0.0 {
                inputs.push(mouse_input(
                    0,
                    0,
                    ((*dy).round() as i32 * WHEEL_DELTA) as u32,
                    MOUSEEVENTF_WHEEL,
                ));
            }
            if *dx != 0.0 {
                inputs.push(mouse_input(
                    0,
                    0,
                    ((*dx).round() as i32 * WHEEL_DELTA) as u32,
                    MOUSEEVENTF_HWHEEL,
                ));
            }
            if inputs.is_empty() {
                Ok(())
            } else {
                windows_send_input(&inputs)
            }
        }
        InputEvent::Key { key, pressed } => {
            if let Some((vk, extended)) = keycode_to_windows_vk(*key) {
                windows_key(vk, *pressed, extended)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_type_text(text: &str) -> Result<()> {
    let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
    for unit in text.encode_utf16() {
        inputs.push(key_input(
            VIRTUAL_KEY(0),
            unit,
            KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0),
        ));
        inputs.push(key_input(
            VIRTUAL_KEY(0),
            unit,
            KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0),
        ));
    }
    if inputs.is_empty() {
        Ok(())
    } else {
        windows_send_input(&inputs)
    }
}

#[cfg(target_os = "windows")]
fn windows_mouse_move(x: i32, y: i32) -> Result<()> {
    let (vx, vy, vw, vh) = windows_virtual_screen();
    let nx = normalize_abs(x, vx, vw);
    let ny = normalize_abs(y, vy, vh);
    windows_send_input(&[mouse_input(
        nx,
        ny,
        0,
        MOUSE_EVENT_FLAGS(MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0),
    )])
}

#[cfg(target_os = "windows")]
fn normalize_abs(value: i32, origin: i32, size: i32) -> i32 {
    let denom = (size - 1).max(1) as i64;
    let raw = ((value - origin) as i64 * 65_535) / denom;
    raw.clamp(0, 65_535) as i32
}

#[cfg(target_os = "windows")]
fn windows_virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

#[cfg(target_os = "windows")]
fn windows_key(vk: VIRTUAL_KEY, pressed: bool, extended: bool) -> Result<()> {
    let mut flags = 0;
    if !pressed {
        flags |= KEYEVENTF_KEYUP.0;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY.0;
    }
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC_EX) as u16 };
    windows_send_input(&[key_input(vk, scan, KEYBD_EVENT_FLAGS(flags))])
}

#[cfg(target_os = "windows")]
fn windows_release_modifiers() {
    for (vk, extended) in [
        (VK_LSHIFT, false),
        (VK_RSHIFT, false),
        (VK_LCONTROL, false),
        (VK_RCONTROL, true),
        (VK_LMENU, false),
        (VK_RMENU, true),
        (VK_LWIN, true),
        (VK_RWIN, true),
    ] {
        let _ = windows_key(vk, false, extended);
    }
}

#[cfg(target_os = "windows")]
fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(target_os = "windows")]
fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(target_os = "windows")]
fn windows_send_input(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "SendInput sent {sent}/{} events: {}",
            inputs.len(),
            windows::core::Error::from_win32()
        ))
    }
}

#[cfg(target_os = "windows")]
pub fn windows_cursor_diagnostics() -> Option<((i32, i32), (i32, i32, i32, i32))> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }
        .ok()
        .map(|()| ((pt.x, pt.y), windows_virtual_screen()))
}

#[cfg(target_os = "windows")]
fn keycode_to_windows_vk(key: KeyCode) -> Option<(VIRTUAL_KEY, bool)> {
    let extended = matches!(
        key,
        KeyCode::Insert
            | KeyCode::Delete
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::RightAlt
            | KeyCode::RightCtrl
            | KeyCode::LeftMeta
            | KeyCode::RightMeta
    );
    let vk = match key {
        KeyCode::A => VK_A,
        KeyCode::B => VK_B,
        KeyCode::C => VK_C,
        KeyCode::D => VK_D,
        KeyCode::E => VK_E,
        KeyCode::F => VK_F,
        KeyCode::G => VK_G,
        KeyCode::H => VK_H,
        KeyCode::I => VK_I,
        KeyCode::J => VK_J,
        KeyCode::K => VK_K,
        KeyCode::L => VK_L,
        KeyCode::M => VK_M,
        KeyCode::N => VK_N,
        KeyCode::O => VK_O,
        KeyCode::P => VK_P,
        KeyCode::Q => VK_Q,
        KeyCode::R => VK_R,
        KeyCode::S => VK_S,
        KeyCode::T => VK_T,
        KeyCode::U => VK_U,
        KeyCode::V => VK_V,
        KeyCode::W => VK_W,
        KeyCode::X => VK_X,
        KeyCode::Y => VK_Y,
        KeyCode::Z => VK_Z,
        KeyCode::Key0 => VK_0,
        KeyCode::Key1 => VK_1,
        KeyCode::Key2 => VK_2,
        KeyCode::Key3 => VK_3,
        KeyCode::Key4 => VK_4,
        KeyCode::Key5 => VK_5,
        KeyCode::Key6 => VK_6,
        KeyCode::Key7 => VK_7,
        KeyCode::Key8 => VK_8,
        KeyCode::Key9 => VK_9,
        KeyCode::F1 => VK_F1,
        KeyCode::F2 => VK_F2,
        KeyCode::F3 => VK_F3,
        KeyCode::F4 => VK_F4,
        KeyCode::F5 => VK_F5,
        KeyCode::F6 => VK_F6,
        KeyCode::F7 => VK_F7,
        KeyCode::F8 => VK_F8,
        KeyCode::F9 => VK_F9,
        KeyCode::F10 => VK_F10,
        KeyCode::F11 => VK_F11,
        KeyCode::F12 => VK_F12,
        KeyCode::LeftShift => VK_LSHIFT,
        KeyCode::RightShift => VK_RSHIFT,
        KeyCode::LeftCtrl => VK_LCONTROL,
        KeyCode::RightCtrl => VK_RCONTROL,
        KeyCode::LeftAlt => VK_LMENU,
        KeyCode::RightAlt => VK_RMENU,
        KeyCode::LeftMeta => VK_LWIN,
        KeyCode::RightMeta => VK_RWIN,
        KeyCode::Up => VK_UP,
        KeyCode::Down => VK_DOWN,
        KeyCode::Left => VK_LEFT,
        KeyCode::Right => VK_RIGHT,
        KeyCode::Home => VK_HOME,
        KeyCode::End => VK_END,
        KeyCode::PageUp => VK_PRIOR,
        KeyCode::PageDown => VK_NEXT,
        KeyCode::Backspace => VK_BACK,
        KeyCode::Delete => VK_DELETE,
        KeyCode::Tab => VK_TAB,
        KeyCode::Enter => VK_RETURN,
        KeyCode::Space => VK_SPACE,
        KeyCode::Escape => VK_ESCAPE,
        KeyCode::Insert => VK_INSERT,
        KeyCode::Minus => VK_OEM_MINUS,
        KeyCode::Equal => VK_OEM_PLUS,
        KeyCode::LeftBracket => VK_OEM_4,
        KeyCode::RightBracket => VK_OEM_6,
        KeyCode::Backslash => VK_OEM_5,
        KeyCode::Semicolon => VK_OEM_1,
        KeyCode::Apostrophe => VK_OEM_7,
        KeyCode::Grave => VK_OEM_3,
        KeyCode::Comma => VK_OEM_COMMA,
        KeyCode::Period => VK_OEM_PERIOD,
        KeyCode::Slash => VK_OEM_2,
        KeyCode::CapsLock => VK_CAPITAL,
        KeyCode::NumLock => VK_NUMLOCK,
        _ => return None,
    };
    Some((vk, extended))
}
