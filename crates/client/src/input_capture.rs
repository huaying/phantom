use phantom_core::input::{InputEvent, KeyCode, MouseButton as PhantomMouseButton};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode as WinitKey, PhysicalKey};

/// Convert winit keyboard event to phantom InputEvent.
pub fn key_event(key: &PhysicalKey, state: ElementState) -> Option<InputEvent> {
    let PhysicalKey::Code(code) = key else { return None };
    let kc = winit_to_keycode(*code)?;
    Some(InputEvent::Key {
        key: kc,
        pressed: state == ElementState::Pressed,
    })
}

/// Convert winit mouse button event.
pub fn mouse_button_event(button: MouseButton, state: ElementState) -> Option<InputEvent> {
    let btn = match button {
        MouseButton::Left => PhantomMouseButton::Left,
        MouseButton::Right => PhantomMouseButton::Right,
        MouseButton::Middle => PhantomMouseButton::Middle,
        _ => return None,
    };
    Some(InputEvent::MouseButton {
        button: btn,
        pressed: state == ElementState::Pressed,
    })
}

/// Convert winit mouse move to phantom InputEvent (caller maps coordinates).
pub fn mouse_move_event(x: i32, y: i32) -> InputEvent {
    InputEvent::MouseMove { x, y }
}

/// Convert winit scroll event.
pub fn scroll_event(delta: MouseScrollDelta) -> Option<InputEvent> {
    let (dx, dy) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, y),
        MouseScrollDelta::PixelDelta(pos) => (pos.x as f32 / 120.0, pos.y as f32 / 120.0),
    };
    if dx.abs() > 0.01 || dy.abs() > 0.01 {
        Some(InputEvent::MouseScroll { dx, dy })
    } else {
        None
    }
}

fn winit_to_keycode(key: WinitKey) -> Option<KeyCode> {
    Some(match key {
        WinitKey::KeyA => KeyCode::A, WinitKey::KeyB => KeyCode::B,
        WinitKey::KeyC => KeyCode::C, WinitKey::KeyD => KeyCode::D,
        WinitKey::KeyE => KeyCode::E, WinitKey::KeyF => KeyCode::F,
        WinitKey::KeyG => KeyCode::G, WinitKey::KeyH => KeyCode::H,
        WinitKey::KeyI => KeyCode::I, WinitKey::KeyJ => KeyCode::J,
        WinitKey::KeyK => KeyCode::K, WinitKey::KeyL => KeyCode::L,
        WinitKey::KeyM => KeyCode::M, WinitKey::KeyN => KeyCode::N,
        WinitKey::KeyO => KeyCode::O, WinitKey::KeyP => KeyCode::P,
        WinitKey::KeyQ => KeyCode::Q, WinitKey::KeyR => KeyCode::R,
        WinitKey::KeyS => KeyCode::S, WinitKey::KeyT => KeyCode::T,
        WinitKey::KeyU => KeyCode::U, WinitKey::KeyV => KeyCode::V,
        WinitKey::KeyW => KeyCode::W, WinitKey::KeyX => KeyCode::X,
        WinitKey::KeyY => KeyCode::Y, WinitKey::KeyZ => KeyCode::Z,

        WinitKey::Digit0 => KeyCode::Key0, WinitKey::Digit1 => KeyCode::Key1,
        WinitKey::Digit2 => KeyCode::Key2, WinitKey::Digit3 => KeyCode::Key3,
        WinitKey::Digit4 => KeyCode::Key4, WinitKey::Digit5 => KeyCode::Key5,
        WinitKey::Digit6 => KeyCode::Key6, WinitKey::Digit7 => KeyCode::Key7,
        WinitKey::Digit8 => KeyCode::Key8, WinitKey::Digit9 => KeyCode::Key9,

        WinitKey::F1 => KeyCode::F1, WinitKey::F2 => KeyCode::F2,
        WinitKey::F3 => KeyCode::F3, WinitKey::F4 => KeyCode::F4,
        WinitKey::F5 => KeyCode::F5, WinitKey::F6 => KeyCode::F6,
        WinitKey::F7 => KeyCode::F7, WinitKey::F8 => KeyCode::F8,
        WinitKey::F9 => KeyCode::F9, WinitKey::F10 => KeyCode::F10,
        WinitKey::F11 => KeyCode::F11, WinitKey::F12 => KeyCode::F12,

        WinitKey::ShiftLeft => KeyCode::LeftShift, WinitKey::ShiftRight => KeyCode::RightShift,
        WinitKey::ControlLeft => KeyCode::LeftCtrl, WinitKey::ControlRight => KeyCode::RightCtrl,
        WinitKey::AltLeft => KeyCode::LeftAlt, WinitKey::AltRight => KeyCode::RightAlt,
        // Don't send Super/Meta to server — macOS Cmd+Tab causes stuck modifier
        // that turns all subsequent keys into GNOME/XFCE shortcuts.

        WinitKey::ArrowUp => KeyCode::Up, WinitKey::ArrowDown => KeyCode::Down,
        WinitKey::ArrowLeft => KeyCode::Left, WinitKey::ArrowRight => KeyCode::Right,
        WinitKey::Home => KeyCode::Home, WinitKey::End => KeyCode::End,
        WinitKey::PageUp => KeyCode::PageUp, WinitKey::PageDown => KeyCode::PageDown,

        WinitKey::Backspace => KeyCode::Backspace, WinitKey::Delete => KeyCode::Delete,
        WinitKey::Tab => KeyCode::Tab, WinitKey::Enter => KeyCode::Enter,
        WinitKey::Space => KeyCode::Space, WinitKey::Escape => KeyCode::Escape,
        WinitKey::Insert => KeyCode::Insert,

        WinitKey::Minus => KeyCode::Minus, WinitKey::Equal => KeyCode::Equal,
        WinitKey::BracketLeft => KeyCode::LeftBracket, WinitKey::BracketRight => KeyCode::RightBracket,
        WinitKey::Backslash => KeyCode::Backslash, WinitKey::Semicolon => KeyCode::Semicolon,
        WinitKey::Quote => KeyCode::Apostrophe, WinitKey::Backquote => KeyCode::Grave,
        WinitKey::Comma => KeyCode::Comma, WinitKey::Period => KeyCode::Period,
        WinitKey::Slash => KeyCode::Slash, WinitKey::CapsLock => KeyCode::CapsLock,
        WinitKey::NumLock => KeyCode::NumLock, WinitKey::ScrollLock => KeyCode::ScrollLock,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mapping_letters() {
        for (winit, expected) in [
            (WinitKey::KeyA, KeyCode::A),
            (WinitKey::KeyZ, KeyCode::Z),
            (WinitKey::KeyM, KeyCode::M),
        ] {
            assert_eq!(winit_to_keycode(winit), Some(expected));
        }
    }

    #[test]
    fn key_mapping_modifiers() {
        assert_eq!(winit_to_keycode(WinitKey::ShiftLeft), Some(KeyCode::LeftShift));
        assert_eq!(winit_to_keycode(WinitKey::ControlLeft), Some(KeyCode::LeftCtrl));
        assert_eq!(winit_to_keycode(WinitKey::AltLeft), Some(KeyCode::LeftAlt));
    }

    #[test]
    fn super_key_not_mapped() {
        // Super/Meta must NOT be mapped — causes stuck modifier on macOS
        assert_eq!(winit_to_keycode(WinitKey::SuperLeft), None);
        assert_eq!(winit_to_keycode(WinitKey::SuperRight), None);
    }

    #[test]
    fn mouse_move_event_coords() {
        let event = mouse_move_event(100, 200);
        match event {
            InputEvent::MouseMove { x, y } => {
                assert_eq!(x, 100);
                assert_eq!(y, 200);
            }
            _ => panic!("expected MouseMove"),
        }
    }

    #[test]
    fn mouse_button_press_release() {
        let press = mouse_button_event(MouseButton::Left, ElementState::Pressed);
        let release = mouse_button_event(MouseButton::Left, ElementState::Released);
        assert!(press.is_some());
        assert!(release.is_some());
        match press.unwrap() {
            InputEvent::MouseButton { pressed, .. } => assert!(pressed),
            _ => panic!("expected MouseButton"),
        }
        match release.unwrap() {
            InputEvent::MouseButton { pressed, .. } => assert!(!pressed),
            _ => panic!("expected MouseButton"),
        }
    }
}
