//! WASAPI loopback capture → Opus encoding (Windows).
//!
//! Captures audio from the default audio render endpoint in loopback mode
//! (i.e. whatever the speakers are playing) and encodes it to Opus at 48kHz
//! stereo.
//!
//! Uses the official `windows` crate for COM/WASAPI APIs.

use super::AudioChunk;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use tracing::{info, warn};

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
// CreateEventW and WaitForSingleObject removed — loopback mode uses polling, not events

/// Handle to a running audio capture thread. Drop to stop capture.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCapture {
    /// Start capturing loopback audio via WASAPI.
    /// Returns a receiver that yields Opus-encoded audio chunks (~20ms each).
    pub fn start() -> Result<(Self, mpsc::Receiver<AudioChunk>)> {
        let sample_rate = 48000u32;
        let channels = 2u8;

        let (tx, rx) = mpsc::sync_channel::<AudioChunk>(50); // ~1s buffer at 20ms/frame
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                if let Err(e) = wasapi_capture_loop(tx, stop_clone, sample_rate, channels) {
                    warn!("WASAPI capture thread exited with error: {e}");
                }
                info!("audio capture thread stopped");
            })
            .context("spawn audio capture thread")?;

        info!(
            sample_rate,
            channels, "WASAPI loopback audio capture started"
        );

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

/// Main WASAPI loopback capture loop. Runs on the audio thread.
fn wasapi_capture_loop(
    tx: mpsc::SyncSender<AudioChunk>,
    stop: Arc<AtomicBool>,
    target_sample_rate: u32,
    target_channels: u8,
) -> Result<()> {
    unsafe {
        // Initialize COM for this thread
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx")?;

        // Get the default audio render endpoint (speakers)
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("create MMDeviceEnumerator")?;

        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("get default audio endpoint")?;

        // Activate IAudioClient on the render device
        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("activate IAudioClient")?;

        // Get the mix format (the format the device is currently using)
        let mix_format_ptr = audio_client.GetMixFormat().context("GetMixFormat")?;
        let mix_format = &*mix_format_ptr;

        let device_sample_rate = mix_format.nSamplesPerSec;
        let device_channels = mix_format.nChannels;
        let bits_per_sample = mix_format.wBitsPerSample;

        info!(
            device_sample_rate,
            device_channels, bits_per_sample, "WASAPI device format"
        );

        // Request 20ms buffer for loopback capture
        // REFERENCE_TIME is in 100ns units; 20ms = 200_000 * 100ns
        let buffer_duration: i64 = 200_000; // 20ms in 100ns units

        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                buffer_duration,
                0, // periodicity (must be 0 for shared mode)
                mix_format_ptr as *const _,
                None, // audio session GUID
            )
            .context("IAudioClient::Initialize")?;

        // Note: SetEventHandle is NOT supported in AUDCLNT_STREAMFLAGS_LOOPBACK mode.
        // Use polling with GetNextPacketSize instead.

        // Get the capture client interface
        let capture_client: IAudioCaptureClient = audio_client
            .GetService()
            .context("GetService<IAudioCaptureClient>")?;

        // Create Opus encoder
        let opus = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )
        .context("create Opus encoder")?;

        // 20ms of audio at 48kHz stereo = 960 frames
        let opus_frame_samples = (target_sample_rate / 50) as usize; // 960
        let mut pcm_accumulator: Vec<i16> =
            Vec::with_capacity(opus_frame_samples * target_channels as usize * 2);
        let mut opus_buf = vec![0u8; 4000];

        // Resampler state (simple linear resampling if device rate != 48kHz)
        let needs_resample = device_sample_rate != target_sample_rate;
        let needs_channel_convert = device_channels as u8 != target_channels;
        let is_float = bits_per_sample == 32; // WASAPI typically uses f32

        // Start capturing
        audio_client.Start().context("IAudioClient::Start")?;
        info!("WASAPI loopback capture started");

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Poll for available audio packets (10ms sleep between polls)
            let packet_size = capture_client.GetNextPacketSize()
                .unwrap_or(0);
            if packet_size == 0 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }

            // Read all available packets
            loop {
                let mut buffer_ptr = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                let mut _device_position = 0u64;
                let mut _qpc_position = 0u64;

                let hr = capture_client.GetBuffer(
                    &mut buffer_ptr,
                    &mut num_frames,
                    &mut flags,
                    Some(&mut _device_position),
                    Some(&mut _qpc_position),
                );

                if hr.is_err() || num_frames == 0 {
                    break;
                }

                let total_samples = num_frames as usize * device_channels as usize;

                // Convert device samples to i16 stereo at 48kHz
                let pcm_i16: Vec<i16> = if is_float {
                    // f32 → i16
                    let f32_slice =
                        std::slice::from_raw_parts(buffer_ptr as *const f32, total_samples);
                    f32_to_i16(f32_slice)
                } else if bits_per_sample == 16 {
                    // Already i16
                    let i16_slice =
                        std::slice::from_raw_parts(buffer_ptr as *const i16, total_samples);
                    i16_slice.to_vec()
                } else if bits_per_sample == 24 {
                    // 24-bit PCM → i16 (drop lower 8 bits)
                    let bytes = std::slice::from_raw_parts(buffer_ptr, total_samples * 3);
                    i24_to_i16(bytes, total_samples)
                } else {
                    // Unsupported format — silence
                    vec![0i16; total_samples]
                };

                // Release the buffer
                let _ = capture_client.ReleaseBuffer(num_frames);

                // Channel conversion (if needed: e.g. mono→stereo or >2ch→stereo)
                let stereo_pcm = if needs_channel_convert {
                    convert_channels(&pcm_i16, device_channels as usize, target_channels as usize)
                } else {
                    pcm_i16
                };

                // Resample if needed (simple linear interpolation)
                let resampled = if needs_resample {
                    resample(
                        &stereo_pcm,
                        target_channels as usize,
                        device_sample_rate,
                        target_sample_rate,
                    )
                } else {
                    stereo_pcm
                };

                // Accumulate samples and encode when we have 20ms worth
                pcm_accumulator.extend_from_slice(&resampled);

                let frame_size = opus_frame_samples * target_channels as usize;
                while pcm_accumulator.len() >= frame_size {
                    let frame: Vec<i16> = pcm_accumulator.drain(..frame_size).collect();

                    match opus.encode(&frame, &mut opus_buf) {
                        Ok(len) => {
                            let chunk = AudioChunk {
                                data: opus_buf[..len].to_vec(),
                                sample_rate: target_sample_rate,
                                channels: target_channels,
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
            }
        }

        // Stop capture
        let _ = audio_client.Stop();
    }

    Ok(())
}

/// Convert f32 samples [-1.0, 1.0] to i16.
fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| {
            let clamped = s.clamp(-1.0, 1.0);
            (clamped * 32767.0) as i16
        })
        .collect()
}

/// Convert 24-bit PCM (packed 3 bytes per sample, little-endian) to i16.
fn i24_to_i16(bytes: &[u8], num_samples: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(num_samples);
    for i in 0..num_samples {
        let offset = i * 3;
        if offset + 2 < bytes.len() {
            // 24-bit LE: [low, mid, high] → take high two bytes as i16
            let hi = bytes[offset + 2] as i16;
            let mid = bytes[offset + 1] as i16;
            out.push((hi << 8) | mid);
        } else {
            out.push(0);
        }
    }
    out
}

/// Convert between channel counts (e.g. mono→stereo, 5.1→stereo).
fn convert_channels(pcm: &[i16], from_channels: usize, to_channels: usize) -> Vec<i16> {
    if from_channels == to_channels {
        return pcm.to_vec();
    }

    let num_frames = pcm.len() / from_channels;
    let mut out = Vec::with_capacity(num_frames * to_channels);

    for frame_idx in 0..num_frames {
        let offset = frame_idx * from_channels;
        if from_channels == 1 && to_channels == 2 {
            // Mono → stereo: duplicate
            let sample = pcm[offset];
            out.push(sample);
            out.push(sample);
        } else if from_channels >= 2 && to_channels == 2 {
            // Multi-channel → stereo: take first two channels
            out.push(pcm[offset]);
            out.push(pcm[offset + 1]);
        } else if from_channels == 2 && to_channels == 1 {
            // Stereo → mono: average
            let avg = ((pcm[offset] as i32 + pcm[offset + 1] as i32) / 2) as i16;
            out.push(avg);
        } else {
            // Fallback: take first N channels
            for ch in 0..to_channels.min(from_channels) {
                out.push(pcm[offset + ch]);
            }
            for _ in from_channels..to_channels {
                out.push(0);
            }
        }
    }

    out
}

/// Simple linear resampling between sample rates.
fn resample(pcm: &[i16], channels: usize, from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate {
        return pcm.to_vec();
    }

    let num_frames_in = pcm.len() / channels;
    let num_frames_out = (num_frames_in as u64 * to_rate as u64 / from_rate as u64) as usize;
    let mut out = Vec::with_capacity(num_frames_out * channels);

    for i in 0..num_frames_out {
        let src_pos = i as f64 * from_rate as f64 / to_rate as f64;
        let src_idx = src_pos as usize;
        let frac = src_pos - src_idx as f64;

        for ch in 0..channels {
            let s0_raw = pcm.get(src_idx * channels + ch).copied().unwrap_or(0);
            let s0 = s0_raw as f64;
            let s1 = pcm
                .get((src_idx + 1) * channels + ch)
                .copied()
                .unwrap_or(s0_raw) as f64;
            let interpolated = s0 + (s1 - s0) * frac;
            out.push(interpolated as i16);
        }
    }

    out
}
