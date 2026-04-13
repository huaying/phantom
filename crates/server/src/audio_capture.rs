//! Cross-platform audio capture → Opus encoding.
//!
//! Dispatches to the platform-specific backend:
//! - **Linux**: PulseAudio monitor source (`audio_capture_pulse`)
//! - **Windows**: WASAPI loopback capture (`audio_capture_wasapi`)

/// Encoded audio chunk ready to be sent as an AudioFrame message.
pub struct AudioChunk {
    pub data: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
}

#[cfg(target_os = "linux")]
#[path = "audio_capture_pulse.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "audio_capture_wasapi.rs"]
mod platform;

pub use platform::AudioCapture;
