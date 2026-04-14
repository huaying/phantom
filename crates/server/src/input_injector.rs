use anyhow::Result;
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use phantom_core::input::{InputEvent, KeyCode, MouseButton};

/// Injects input events into the OS via enigo.
/// macOS: requires Accessibility permission.
pub struct InputInjector {
    enigo: Enigo,
}

impl InputInjector {
    pub fn new() -> Result<Self> {
        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| anyhow::anyhow!("failed to init enigo: {e}"))?;

        // Release all modifier keys to clear any stuck state from previous sessions
        for key in [Key::Shift, Key::Control, Key::Alt, Key::Meta] {
            let _ = enigo.key(key, Direction::Release);
        }

        tracing::info!("InputInjector initialized");
        Ok(Self { enigo })
    }

    /// Type out a string (for paste operations).
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        self.enigo
            .text(text)
            .map_err(|e| anyhow::anyhow!("type text: {e}"))?;
        Ok(())
    }

    pub fn inject(&mut self, event: &InputEvent) -> Result<()> {
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
                if let Some(enigo_key) = keycode_to_enigo(*key) {
                    let dir = if *pressed {
                        Direction::Press
                    } else {
                        Direction::Release
                    };
                    self.enigo
                        .key(enigo_key, dir)
                        .map_err(|e| anyhow::anyhow!("key: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

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
