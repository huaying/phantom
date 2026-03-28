use minifb::{Key, MouseMode, Window};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};
use std::collections::HashSet;

/// Captures mouse and keyboard input from a minifb window.
/// Tracks state to only emit press/release/move events on changes.
pub struct InputCapture {
    prev_mouse_pos: (f32, f32),
    prev_mouse_buttons: [bool; 3],
    prev_keys: HashSet<Key>,
}

impl InputCapture {
    pub fn new() -> Self {
        Self {
            prev_mouse_pos: (-1.0, -1.0),
            prev_mouse_buttons: [false; 3],
            prev_keys: HashSet::new(),
        }
    }

    /// Poll the window for input changes.
    /// `map_coords` converts window-space mouse coordinates to server-space.
    pub fn poll(
        &mut self,
        window: &Window,
        map_coords: impl Fn(f32, f32) -> (i32, i32),
    ) -> Vec<InputEvent> {
        let mut events = Vec::new();

        // -- Mouse position --
        if let Some((x, y)) = window.get_mouse_pos(MouseMode::Clamp) {
            if (x - self.prev_mouse_pos.0).abs() > 0.5
                || (y - self.prev_mouse_pos.1).abs() > 0.5
            {
                let (sx, sy) = map_coords(x, y);
                events.push(InputEvent::MouseMove { x: sx, y: sy });
                self.prev_mouse_pos = (x, y);
            }
        }

        // -- Mouse buttons --
        let buttons = [
            (minifb::MouseButton::Left, MouseButton::Left),
            (minifb::MouseButton::Right, MouseButton::Right),
            (minifb::MouseButton::Middle, MouseButton::Middle),
        ];
        for (i, (mfb_btn, phantom_btn)) in buttons.iter().enumerate() {
            let down = window.get_mouse_down(*mfb_btn);
            if down != self.prev_mouse_buttons[i] {
                events.push(InputEvent::MouseButton {
                    button: *phantom_btn,
                    pressed: down,
                });
                self.prev_mouse_buttons[i] = down;
            }
        }

        // -- Mouse scroll --
        if let Some((dx, dy)) = window.get_scroll_wheel() {
            if dx.abs() > 0.01 || dy.abs() > 0.01 {
                events.push(InputEvent::MouseScroll { dx, dy });
            }
        }

        // -- Keyboard --
        let keys: HashSet<Key> = window.get_keys().into_iter().collect();

        // Newly pressed
        for &key in &keys {
            if !self.prev_keys.contains(&key) {
                if let Some(kc) = minifb_to_keycode(key) {
                    events.push(InputEvent::Key {
                        key: kc,
                        pressed: true,
                    });
                }
            }
        }

        // Newly released
        for &key in &self.prev_keys {
            if !keys.contains(&key) {
                if let Some(kc) = minifb_to_keycode(key) {
                    events.push(InputEvent::Key {
                        key: kc,
                        pressed: false,
                    });
                }
            }
        }

        self.prev_keys = keys;
        events
    }
}

fn minifb_to_keycode(key: Key) -> Option<KeyCode> {
    Some(match key {
        Key::A => KeyCode::A,
        Key::B => KeyCode::B,
        Key::C => KeyCode::C,
        Key::D => KeyCode::D,
        Key::E => KeyCode::E,
        Key::F => KeyCode::F,
        Key::G => KeyCode::G,
        Key::H => KeyCode::H,
        Key::I => KeyCode::I,
        Key::J => KeyCode::J,
        Key::K => KeyCode::K,
        Key::L => KeyCode::L,
        Key::M => KeyCode::M,
        Key::N => KeyCode::N,
        Key::O => KeyCode::O,
        Key::P => KeyCode::P,
        Key::Q => KeyCode::Q,
        Key::R => KeyCode::R,
        Key::S => KeyCode::S,
        Key::T => KeyCode::T,
        Key::U => KeyCode::U,
        Key::V => KeyCode::V,
        Key::W => KeyCode::W,
        Key::X => KeyCode::X,
        Key::Y => KeyCode::Y,
        Key::Z => KeyCode::Z,

        Key::Key0 => KeyCode::Key0,
        Key::Key1 => KeyCode::Key1,
        Key::Key2 => KeyCode::Key2,
        Key::Key3 => KeyCode::Key3,
        Key::Key4 => KeyCode::Key4,
        Key::Key5 => KeyCode::Key5,
        Key::Key6 => KeyCode::Key6,
        Key::Key7 => KeyCode::Key7,
        Key::Key8 => KeyCode::Key8,
        Key::Key9 => KeyCode::Key9,

        Key::F1 => KeyCode::F1,
        Key::F2 => KeyCode::F2,
        Key::F3 => KeyCode::F3,
        Key::F4 => KeyCode::F4,
        Key::F5 => KeyCode::F5,
        Key::F6 => KeyCode::F6,
        Key::F7 => KeyCode::F7,
        Key::F8 => KeyCode::F8,
        Key::F9 => KeyCode::F9,
        Key::F10 => KeyCode::F10,
        Key::F11 => KeyCode::F11,
        Key::F12 => KeyCode::F12,

        Key::LeftShift => KeyCode::LeftShift,
        Key::RightShift => KeyCode::RightShift,
        Key::LeftCtrl => KeyCode::LeftCtrl,
        Key::RightCtrl => KeyCode::RightCtrl,
        Key::LeftAlt => KeyCode::LeftAlt,
        Key::RightAlt => KeyCode::RightAlt,
        Key::LeftSuper => KeyCode::LeftMeta,
        Key::RightSuper => KeyCode::RightMeta,

        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,

        Key::Backspace => KeyCode::Backspace,
        Key::Delete => KeyCode::Delete,
        Key::Tab => KeyCode::Tab,
        Key::Enter => KeyCode::Enter,
        Key::Space => KeyCode::Space,
        Key::Escape => KeyCode::Escape,
        Key::Insert => KeyCode::Insert,

        Key::Minus => KeyCode::Minus,
        Key::Equal => KeyCode::Equal,
        Key::LeftBracket => KeyCode::LeftBracket,
        Key::RightBracket => KeyCode::RightBracket,
        Key::Backslash => KeyCode::Backslash,
        Key::Semicolon => KeyCode::Semicolon,
        Key::Apostrophe => KeyCode::Apostrophe,
        Key::Backquote => KeyCode::Grave,
        Key::Comma => KeyCode::Comma,
        Key::Period => KeyCode::Period,
        Key::Slash => KeyCode::Slash,
        Key::CapsLock => KeyCode::CapsLock,
        Key::NumLock => KeyCode::NumLock,
        Key::ScrollLock => KeyCode::ScrollLock,

        _ => return None,
    })
}
