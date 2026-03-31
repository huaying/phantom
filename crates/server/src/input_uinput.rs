//! Input injection via Linux uinput — works on GNOME/Mutter (unlike XTest/enigo).
//! Creates a virtual input device at the kernel level. Requires /dev/uinput access.

use anyhow::{Context, Result};
use input_linux::sys::input_event;
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis,
    UInputHandle,
};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
use std::fs::OpenOptions;



pub struct UInputInjector {
    handle: UInputHandle<std::fs::File>,
}

impl UInputInjector {
    pub fn new(_frame_width: u32, _frame_height: u32) -> Result<Self> {
        // Use X virtual screen size (not frame size) for uinput absolute coordinate range.
        // On multi-monitor setups, virtual screen can be wider than the captured frame.
        let (screen_width, screen_height) = get_x_screen_size()
            .unwrap_or((_frame_width, _frame_height));
        tracing::info!(screen_width, screen_height, "uinput using X screen dimensions");

        let f = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("open /dev/uinput (chmod 666 /dev/uinput)")?;

        let handle = UInputHandle::new(f);

        // Enable event types
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Relative)?;
        handle.set_evbit(EventKind::Synchronize)?;

        // Mouse buttons
        handle.set_keybit(Key::ButtonLeft)?;
        handle.set_keybit(Key::ButtonRight)?;
        handle.set_keybit(Key::ButtonMiddle)?;

        // Keyboard keys
        for key in ALL_KEYS {
            handle.set_keybit(*key)?;
        }

        // Absolute axes
        handle.set_absbit(AbsoluteAxis::X)?;
        handle.set_absbit(AbsoluteAxis::Y)?;

        // Relative axes (scroll)
        handle.set_relbit(RelativeAxis::Wheel)?;
        handle.set_relbit(RelativeAxis::HorizontalWheel)?;

        let id = InputId {
            bustype: input_linux::sys::BUS_USB,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
        };

        let abs = [
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::X,
                info: AbsoluteInfo {
                    value: 0,
                    minimum: 0,
                    maximum: screen_width as i32 - 1,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            },
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::Y,
                info: AbsoluteInfo {
                    value: 0,
                    minimum: 0,
                    maximum: screen_height as i32 - 1,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            },
        ];

        handle.create(&id, b"Phantom Virtual Input\0", 0, &abs)?;

        // Wait for device to register
        std::thread::sleep(std::time::Duration::from_millis(200));

        tracing::info!(screen_width, screen_height, "UInput injector initialized");
        Ok(Self { handle })
    }

    pub fn type_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if let Some((key, shift)) = char_to_key(ch) {
                if shift {
                    self.write_key(Key::LeftShift, 1)?;
                }
                self.write_key(key, 1)?;
                self.write_key(key, 0)?;
                if shift {
                    self.write_key(Key::LeftShift, 0)?;
                }
            }
        }
        Ok(())
    }

    pub fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y } => {
                self.write_abs(AbsoluteAxis::X, *x)?;
                self.write_abs(AbsoluteAxis::Y, *y)?;
                self.write_syn()?;
            }
            InputEvent::MouseButton { button, pressed } => {
                let btn = match button {
                    MouseButton::Left => Key::ButtonLeft,
                    MouseButton::Right => Key::ButtonRight,
                    MouseButton::Middle => Key::ButtonMiddle,
                };
                self.write_key(btn, if *pressed { 1 } else { 0 })?;
                self.write_syn()?;
            }
            InputEvent::MouseScroll { dx, dy } => {
                if *dy != 0.0 {
                    self.write_rel(RelativeAxis::Wheel, (*dy * 3.0) as i32)?;
                }
                if *dx != 0.0 {
                    self.write_rel(RelativeAxis::HorizontalWheel, (*dx * 3.0) as i32)?;
                }
                self.write_syn()?;
            }
            InputEvent::Key { key, pressed } => {
                if let Some(k) = keycode_to_linux(*key) {
                    self.write_key(k, if *pressed { 1 } else { 0 })?;
                    self.write_syn()?;
                }
            }
        }
        Ok(())
    }

    fn write_abs(&self, axis: AbsoluteAxis, value: i32) -> Result<()> {
        let ev = input_event {
            time: input_linux::sys::timeval { tv_sec: 0, tv_usec: 0 },
            type_: input_linux::sys::EV_ABS as u16,
            code: axis as u16,
            value,
        };
        self.handle.write(&[ev])?;
        Ok(())
    }

    fn write_key(&self, key: Key, value: i32) -> Result<()> {
        let ev = input_event {
            time: input_linux::sys::timeval { tv_sec: 0, tv_usec: 0 },
            type_: input_linux::sys::EV_KEY as u16,
            code: key as u16,
            value,
        };
        self.handle.write(&[ev])?;
        Ok(())
    }

    fn write_rel(&self, axis: RelativeAxis, value: i32) -> Result<()> {
        let ev = input_event {
            time: input_linux::sys::timeval { tv_sec: 0, tv_usec: 0 },
            type_: input_linux::sys::EV_REL as u16,
            code: axis as u16,
            value,
        };
        self.handle.write(&[ev])?;
        Ok(())
    }

    fn write_syn(&self) -> Result<()> {
        let ev = input_event {
            time: input_linux::sys::timeval { tv_sec: 0, tv_usec: 0 },
            type_: input_linux::sys::EV_SYN as u16,
            code: input_linux::sys::SYN_REPORT as u16,
            value: 0,
        };
        self.handle.write(&[ev])?;
        Ok(())
    }
}

fn keycode_to_linux(key: KeyCode) -> Option<Key> {
    Some(match key {
        KeyCode::A => Key::A, KeyCode::B => Key::B, KeyCode::C => Key::C,
        KeyCode::D => Key::D, KeyCode::E => Key::E, KeyCode::F => Key::F,
        KeyCode::G => Key::G, KeyCode::H => Key::H, KeyCode::I => Key::I,
        KeyCode::J => Key::J, KeyCode::K => Key::K, KeyCode::L => Key::L,
        KeyCode::M => Key::M, KeyCode::N => Key::N, KeyCode::O => Key::O,
        KeyCode::P => Key::P, KeyCode::Q => Key::Q, KeyCode::R => Key::R,
        KeyCode::S => Key::S, KeyCode::T => Key::T, KeyCode::U => Key::U,
        KeyCode::V => Key::V, KeyCode::W => Key::W, KeyCode::X => Key::X,
        KeyCode::Y => Key::Y, KeyCode::Z => Key::Z,
        KeyCode::Key0 => Key::Num0, KeyCode::Key1 => Key::Num1, KeyCode::Key2 => Key::Num2,
        KeyCode::Key3 => Key::Num3, KeyCode::Key4 => Key::Num4, KeyCode::Key5 => Key::Num5,
        KeyCode::Key6 => Key::Num6, KeyCode::Key7 => Key::Num7, KeyCode::Key8 => Key::Num8,
        KeyCode::Key9 => Key::Num9,
        KeyCode::F1 => Key::F1, KeyCode::F2 => Key::F2, KeyCode::F3 => Key::F3,
        KeyCode::F4 => Key::F4, KeyCode::F5 => Key::F5, KeyCode::F6 => Key::F6,
        KeyCode::F7 => Key::F7, KeyCode::F8 => Key::F8, KeyCode::F9 => Key::F9,
        KeyCode::F10 => Key::F10, KeyCode::F11 => Key::F11, KeyCode::F12 => Key::F12,
        KeyCode::LeftShift => Key::LeftShift, KeyCode::RightShift => Key::RightShift,
        KeyCode::LeftCtrl => Key::LeftCtrl, KeyCode::RightCtrl => Key::RightCtrl,
        KeyCode::LeftAlt => Key::LeftAlt, KeyCode::RightAlt => Key::RightAlt,
        KeyCode::LeftMeta => Key::LeftMeta, KeyCode::RightMeta => Key::RightMeta,
        KeyCode::Up => Key::Up, KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left, KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home, KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp, KeyCode::PageDown => Key::PageDown,
        KeyCode::Backspace => Key::Backspace, KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab, KeyCode::Enter => Key::Enter,
        KeyCode::Space => Key::Space, KeyCode::Escape => Key::Esc,
        KeyCode::Minus => Key::Minus, KeyCode::Equal => Key::Equal,
        KeyCode::LeftBracket => Key::LeftBrace, KeyCode::RightBracket => Key::RightBrace,
        KeyCode::Backslash => Key::Backslash, KeyCode::Semicolon => Key::Semicolon,
        KeyCode::Apostrophe => Key::Apostrophe, KeyCode::Grave => Key::Grave,
        KeyCode::Comma => Key::Comma, KeyCode::Period => Key::Dot, KeyCode::Slash => Key::Slash,
        KeyCode::CapsLock => Key::CapsLock,
        _ => return None,
    })
}

fn char_to_key(ch: char) -> Option<(Key, bool)> {
    let (kc, shift) = match ch {
        'a'..='z' => { let idx = ch as u8 - b'a'; (ALL_ALPHA[idx as usize], false) }
        'A'..='Z' => { let idx = ch as u8 - b'A'; (ALL_ALPHA[idx as usize], true) }
        '0' => (Key::Num0, false), '1' => (Key::Num1, false), '2' => (Key::Num2, false),
        '3' => (Key::Num3, false), '4' => (Key::Num4, false), '5' => (Key::Num5, false),
        '6' => (Key::Num6, false), '7' => (Key::Num7, false), '8' => (Key::Num8, false),
        '9' => (Key::Num9, false),
        ' ' => (Key::Space, false), '\n' => (Key::Enter, false), '\t' => (Key::Tab, false),
        '-' => (Key::Minus, false), '=' => (Key::Equal, false),
        ',' => (Key::Comma, false), '.' => (Key::Dot, false), '/' => (Key::Slash, false),
        _ => return None,
    };
    Some((kc, shift))
}

const ALL_ALPHA: [Key; 26] = [
    Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H,
    Key::I, Key::J, Key::K, Key::L, Key::M, Key::N, Key::O, Key::P,
    Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X,
    Key::Y, Key::Z,
];

const ALL_KEYS: &[Key] = &[
    Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H,
    Key::I, Key::J, Key::K, Key::L, Key::M, Key::N, Key::O, Key::P,
    Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X,
    Key::Y, Key::Z,
    Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4,
    Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9,
    Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6,
    Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12,
    Key::LeftShift, Key::RightShift, Key::LeftCtrl, Key::RightCtrl,
    Key::LeftAlt, Key::RightAlt, Key::LeftMeta, Key::RightMeta,
    Key::Up, Key::Down, Key::Left, Key::Right,
    Key::Home, Key::End, Key::PageUp, Key::PageDown,
    Key::Backspace, Key::Delete, Key::Tab, Key::Enter, Key::Space, Key::Esc,
    Key::Minus, Key::Equal, Key::LeftBrace, Key::RightBrace, Key::Backslash,
    Key::Semicolon, Key::Apostrophe, Key::Grave, Key::Comma, Key::Dot, Key::Slash,
    Key::CapsLock,
];

/// Get the X virtual screen dimensions via `xdpyinfo`.
fn get_x_screen_size() -> Option<(u32, u32)> {
    let output = std::process::Command::new("xdpyinfo")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("dimensions:") {
            // "  dimensions:    2944x1080 pixels (778x285 millimeters)"
            let dims = line.split_whitespace().nth(1)?;
            let mut parts = dims.split('x');
            let w: u32 = parts.next()?.parse().ok()?;
            let h: u32 = parts.next()?.parse().ok()?;
            return Some((w, h));
        }
    }
    None
}
