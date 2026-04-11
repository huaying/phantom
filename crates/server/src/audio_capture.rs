//! PulseAudio monitor capture → Opus encoding.
//!
//! Captures audio from the default PulseAudio monitor source (which mirrors
//! whatever is playing on the desktop) and encodes it to Opus at 48kHz stereo.
//!
//! The audio thread runs independently from the video session loop and sends
//! encoded Opus frames through an mpsc channel.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use tracing::{info, warn};

/// Encoded audio chunk ready to be sent as an AudioFrame message.
pub struct AudioChunk {
    pub data: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Handle to a running audio capture thread. Drop to stop capture.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCapture {
    /// Start capturing audio from PulseAudio monitor source.
    /// Returns a receiver that yields Opus-encoded audio chunks (~20ms each).
    pub fn start() -> Result<(Self, mpsc::Receiver<AudioChunk>)> {
        use libpulse_binding as pulse;
        use libpulse_simple_binding as psimple;

        let sample_rate = 48000u32;
        let channels = 2u8;

        // 20ms of audio at 48kHz stereo = 960 frames × 2 channels × 2 bytes = 3840 bytes
        let frame_samples = (sample_rate / 50) as usize; // 960 samples per channel per 20ms
        let pcm_buf_size = frame_samples * channels as usize * 2; // 16-bit PCM

        // Connect to PulseAudio monitor source
        let spec = pulse::sample::Spec {
            format: pulse::sample::Format::S16le,
            channels,
            rate: sample_rate,
        };
        assert!(spec.is_valid());

        // Try to find the monitor source for the default sink.
        // Format: "<sink_name>.monitor" — capture desktop audio output.
        let source_name = find_monitor_source();
        let source_ref = source_name.as_deref();

        let pa = psimple::Simple::new(
            None,                             // default server
            "phantom-server",                 // app name
            pulse::stream::Direction::Record, // recording
            source_ref,                       // device: monitor source or None for default
            "desktop-audio",                  // stream description
            &spec,
            None, // default channel map
            None, // default buffer attrs
        )
        .map_err(|e| anyhow::anyhow!("PulseAudio connect failed: {e}"))?;

        info!(
            source = source_ref.unwrap_or("default"),
            sample_rate, channels, "audio capture started"
        );

        // Create Opus encoder
        let opus = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )
        .context("create Opus encoder")?;

        let (tx, rx) = mpsc::sync_channel::<AudioChunk>(50); // ~1s buffer at 20ms/frame
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                let mut pcm_buf = vec![0u8; pcm_buf_size];
                let mut opus_buf = vec![0u8; 4000]; // max Opus frame size

                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    // Read 20ms of PCM from PulseAudio
                    if let Err(e) = pa.read(&mut pcm_buf) {
                        warn!("PulseAudio read error: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }

                    // Convert u8 buffer to i16 slice for Opus
                    let pcm_i16: &[i16] = unsafe {
                        std::slice::from_raw_parts(
                            pcm_buf.as_ptr() as *const i16,
                            pcm_buf.len() / 2,
                        )
                    };

                    // Encode to Opus
                    match opus.encode(pcm_i16, &mut opus_buf) {
                        Ok(len) => {
                            let chunk = AudioChunk {
                                data: opus_buf[..len].to_vec(),
                                sample_rate,
                                channels,
                            };
                            if tx.try_send(chunk).is_err() {
                                // Receiver dropped or buffer full — skip frame
                            }
                        }
                        Err(e) => {
                            warn!("Opus encode error: {e}");
                        }
                    }
                }

                info!("audio capture thread stopped");
            })
            .context("spawn audio capture thread")?;

        Ok((
            AudioCapture {
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Try to find the PulseAudio monitor source name.
/// Returns `Some("<sink>.monitor")` or `None` for default.
fn find_monitor_source() -> Option<String> {
    // Use pactl to find the default sink, then derive monitor name
    let output = std::process::Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sink = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sink.is_empty() {
        return None;
    }
    let monitor = format!("{sink}.monitor");
    info!(monitor = %monitor, "found PulseAudio monitor source");
    Some(monitor)
}
