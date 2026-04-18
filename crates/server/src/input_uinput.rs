//! Linux uinput-based keyboard injection.
//!
//! Creates a virtual keyboard device under /dev/uinput that the kernel
//! treats exactly like a real USB keyboard. Events go through the
//! standard input subsystem (libinput → X server → XKB → app), so:
//!
//! - GDM login screen works (XTest-based injection races against
//!   MappingNotify on GDM 42; uinput sidesteps that entirely).
//! - Wayland apps receive our events (XTest doesn't reach Wayland).
//! - Lock screen + any greeter works.
//! - The server's own `setxkbmap` layout applies naturally — we send
//!   physical scancodes and X/Wayland translates to keysyms.
//!
//! This mirrors Sunshine's approach (src/platform/linux/input/inputtino_
//! keyboard.cpp) and the uinput branch of RustDesk.
//!
//! Requires write access to /dev/uinput. Install.sh ships a udev rule
//! giving the `input` group rw, and adds the invoking user to that
//! group. If opening fails, the caller falls back to enigo/XTest.
//!
//! Scope (intentional): keyboard only. Mouse + scroll continue through
//! enigo because (a) they don't hit the XKB remap race, and (b) abs
//! pointer uinput needs a more involved virtual-tablet setup. Worth
//! revisiting later for Wayland mouse support.

use anyhow::{anyhow, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent as EvdevEvent, KeyCode as Key};
use phantom_core::input::KeyCode;

pub struct UinputKeyboard {
    device: VirtualDevice,
}

impl UinputKeyboard {
    /// Open /dev/uinput and create the virtual keyboard device.
    /// Returns Err if we can't access uinput (unprivileged + no udev rule).
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        for k in ALL_SUPPORTED_KEYS {
            keys.insert(*k);
        }

        let device = VirtualDevice::builder()
            .context("open /dev/uinput (need udev rule or root)")?
            .name("Phantom Virtual Keyboard")
            .with_keys(&keys)
            .context("register keys on virtual device")?
            .build()
            .context("build virtual keyboard")?;

        tracing::info!("uinput virtual keyboard created");
        Ok(Self { device })
    }

    /// Inject a single key press or release via the virtual device.
    pub fn inject_key(&mut self, key: KeyCode, pressed: bool) -> Result<()> {
        let evdev_key = keycode_to_evdev(key)
            .ok_or_else(|| anyhow!("no evdev mapping for {key:?}"))?;
        // EV_KEY: value 1 = press, 0 = release, 2 = auto-repeat
        // (we never send 2 — OS handles key repeat itself).
        let value = if pressed { 1 } else { 0 };
        let ev = EvdevEvent::new(EventType::KEY.0, evdev_key.code(), value);
        self.device
            .emit(&[ev])
            .context("emit evdev key event")?;
        Ok(())
    }
}

/// Map phantom's platform-agnostic KeyCode to an evdev Key (Linux
/// scancode). Physical key position, not keysym — the server's X /
/// Wayland session is free to apply whatever layout it wants on top.
fn keycode_to_evdev(key: KeyCode) -> Option<Key> {
    use KeyCode as K;
    Some(match key {
        K::A => Key::KEY_A,
        K::B => Key::KEY_B,
        K::C => Key::KEY_C,
        K::D => Key::KEY_D,
        K::E => Key::KEY_E,
        K::F => Key::KEY_F,
        K::G => Key::KEY_G,
        K::H => Key::KEY_H,
        K::I => Key::KEY_I,
        K::J => Key::KEY_J,
        K::K => Key::KEY_K,
        K::L => Key::KEY_L,
        K::M => Key::KEY_M,
        K::N => Key::KEY_N,
        K::O => Key::KEY_O,
        K::P => Key::KEY_P,
        K::Q => Key::KEY_Q,
        K::R => Key::KEY_R,
        K::S => Key::KEY_S,
        K::T => Key::KEY_T,
        K::U => Key::KEY_U,
        K::V => Key::KEY_V,
        K::W => Key::KEY_W,
        K::X => Key::KEY_X,
        K::Y => Key::KEY_Y,
        K::Z => Key::KEY_Z,

        K::Key0 => Key::KEY_0,
        K::Key1 => Key::KEY_1,
        K::Key2 => Key::KEY_2,
        K::Key3 => Key::KEY_3,
        K::Key4 => Key::KEY_4,
        K::Key5 => Key::KEY_5,
        K::Key6 => Key::KEY_6,
        K::Key7 => Key::KEY_7,
        K::Key8 => Key::KEY_8,
        K::Key9 => Key::KEY_9,

        K::F1 => Key::KEY_F1,
        K::F2 => Key::KEY_F2,
        K::F3 => Key::KEY_F3,
        K::F4 => Key::KEY_F4,
        K::F5 => Key::KEY_F5,
        K::F6 => Key::KEY_F6,
        K::F7 => Key::KEY_F7,
        K::F8 => Key::KEY_F8,
        K::F9 => Key::KEY_F9,
        K::F10 => Key::KEY_F10,
        K::F11 => Key::KEY_F11,
        K::F12 => Key::KEY_F12,

        K::LeftShift => Key::KEY_LEFTSHIFT,
        K::RightShift => Key::KEY_RIGHTSHIFT,
        K::LeftCtrl => Key::KEY_LEFTCTRL,
        K::RightCtrl => Key::KEY_RIGHTCTRL,
        K::LeftAlt => Key::KEY_LEFTALT,
        K::RightAlt => Key::KEY_RIGHTALT,
        K::LeftMeta => Key::KEY_LEFTMETA,
        K::RightMeta => Key::KEY_RIGHTMETA,

        K::Up => Key::KEY_UP,
        K::Down => Key::KEY_DOWN,
        K::Left => Key::KEY_LEFT,
        K::Right => Key::KEY_RIGHT,
        K::Home => Key::KEY_HOME,
        K::End => Key::KEY_END,
        K::PageUp => Key::KEY_PAGEUP,
        K::PageDown => Key::KEY_PAGEDOWN,

        K::Backspace => Key::KEY_BACKSPACE,
        K::Delete => Key::KEY_DELETE,
        K::Tab => Key::KEY_TAB,
        K::Enter => Key::KEY_ENTER,
        K::Space => Key::KEY_SPACE,
        K::Escape => Key::KEY_ESC,
        K::Insert => Key::KEY_INSERT,

        K::Minus => Key::KEY_MINUS,
        K::Equal => Key::KEY_EQUAL,
        K::LeftBracket => Key::KEY_LEFTBRACE,
        K::RightBracket => Key::KEY_RIGHTBRACE,
        K::Backslash => Key::KEY_BACKSLASH,
        K::Semicolon => Key::KEY_SEMICOLON,
        K::Apostrophe => Key::KEY_APOSTROPHE,
        K::Grave => Key::KEY_GRAVE,
        K::Comma => Key::KEY_COMMA,
        K::Period => Key::KEY_DOT,
        K::Slash => Key::KEY_SLASH,

        K::CapsLock => Key::KEY_CAPSLOCK,
        K::NumLock => Key::KEY_NUMLOCK,
        K::ScrollLock => Key::KEY_SCROLLLOCK,
        K::PrintScreen => Key::KEY_PRINT,
        K::Pause => Key::KEY_PAUSE,
    })
}

/// All keys we register on the virtual device. Must be a superset of
/// whatever `keycode_to_evdev` might return, otherwise the kernel
/// rejects emit() for unregistered keys.
const ALL_SUPPORTED_KEYS: &[Key] = &[
    Key::KEY_A, Key::KEY_B, Key::KEY_C, Key::KEY_D, Key::KEY_E, Key::KEY_F,
    Key::KEY_G, Key::KEY_H, Key::KEY_I, Key::KEY_J, Key::KEY_K, Key::KEY_L,
    Key::KEY_M, Key::KEY_N, Key::KEY_O, Key::KEY_P, Key::KEY_Q, Key::KEY_R,
    Key::KEY_S, Key::KEY_T, Key::KEY_U, Key::KEY_V, Key::KEY_W, Key::KEY_X,
    Key::KEY_Y, Key::KEY_Z,
    Key::KEY_0, Key::KEY_1, Key::KEY_2, Key::KEY_3, Key::KEY_4,
    Key::KEY_5, Key::KEY_6, Key::KEY_7, Key::KEY_8, Key::KEY_9,
    Key::KEY_F1, Key::KEY_F2, Key::KEY_F3, Key::KEY_F4, Key::KEY_F5, Key::KEY_F6,
    Key::KEY_F7, Key::KEY_F8, Key::KEY_F9, Key::KEY_F10, Key::KEY_F11, Key::KEY_F12,
    Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT,
    Key::KEY_LEFTCTRL, Key::KEY_RIGHTCTRL,
    Key::KEY_LEFTALT, Key::KEY_RIGHTALT,
    Key::KEY_LEFTMETA, Key::KEY_RIGHTMETA,
    Key::KEY_UP, Key::KEY_DOWN, Key::KEY_LEFT, Key::KEY_RIGHT,
    Key::KEY_HOME, Key::KEY_END, Key::KEY_PAGEUP, Key::KEY_PAGEDOWN,
    Key::KEY_BACKSPACE, Key::KEY_DELETE, Key::KEY_TAB, Key::KEY_ENTER,
    Key::KEY_SPACE, Key::KEY_ESC, Key::KEY_INSERT,
    Key::KEY_MINUS, Key::KEY_EQUAL, Key::KEY_LEFTBRACE, Key::KEY_RIGHTBRACE,
    Key::KEY_BACKSLASH, Key::KEY_SEMICOLON, Key::KEY_APOSTROPHE,
    Key::KEY_GRAVE, Key::KEY_COMMA, Key::KEY_DOT, Key::KEY_SLASH,
    Key::KEY_CAPSLOCK, Key::KEY_NUMLOCK, Key::KEY_SCROLLLOCK,
    Key::KEY_PRINT, Key::KEY_PAUSE,
];
