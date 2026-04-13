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

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use platform::AudioCapture;

/// Stub for unsupported platforms (macOS etc.)
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub struct AudioCapture;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl AudioCapture {
    pub fn start() -> anyhow::Result<(Self, std::sync::mpsc::Receiver<AudioChunk>)> {
        anyhow::bail!("audio capture not supported on this platform")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl Drop for AudioCapture {
    fn drop(&mut self) {}
}
