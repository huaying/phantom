//! Opus decode → cpal audio playback.
//!
//! Receives Opus-encoded audio chunks from the server and plays them through
//! the default audio output device using cpal.

use anyhow::{Context, Result};
use std::sync::mpsc;
use tracing::{info, warn};

/// Start the audio playback pipeline. Returns a sender that accepts raw Opus
/// packets. Drop the sender to stop playback.
pub fn start_playback(
    sample_rate: u32,
    channels: u8,
) -> Result<mpsc::SyncSender<Vec<u8>>> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let opus_channels = match channels {
        1 => audiopus::Channels::Mono,
        2 => audiopus::Channels::Stereo,
        _ => anyhow::bail!("unsupported channel count: {channels}"),
    };
    let opus_sample_rate = match sample_rate {
        48000 => audiopus::SampleRate::Hz48000,
        24000 => audiopus::SampleRate::Hz24000,
        16000 => audiopus::SampleRate::Hz16000,
        12000 => audiopus::SampleRate::Hz12000,
        8000 => audiopus::SampleRate::Hz8000,
        _ => anyhow::bail!("unsupported sample rate for Opus: {sample_rate}"),
    };

    let mut decoder = audiopus::coder::Decoder::new(opus_sample_rate, opus_channels)
        .context("create Opus decoder")?;

    // Ring buffer: decoded PCM samples flow from decoder thread → cpal callback.
    // Size: ~200ms of audio at 48kHz stereo (enough to absorb jitter).
    let ring_size = (sample_rate as usize) * (channels as usize) * 200 / 1000;
    let ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<f32>::with_capacity(ring_size)));
    let ring_writer = std::sync::Arc::clone(&ring);

    // Opus packets come in here
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(100);

    // Decoder thread: Opus → PCM → ring buffer
    let ch = channels;
    std::thread::Builder::new()
        .name("audio-decode".into())
        .spawn(move || {
            // Opus decodes to i16, we convert to f32 for cpal
            let frame_samples = 960; // 20ms at 48kHz
            let mut pcm_i16 = vec![0i16; frame_samples * ch as usize];

            while let Ok(opus_data) = rx.recv() {
                let out = audiopus::MutSignals::try_from(&mut pcm_i16[..]).unwrap();
                match decoder.decode(
                    Some(audiopus::packet::Packet::try_from(opus_data.as_slice()).unwrap()),
                    out,
                    false, // no FEC
                ) {
                    Ok(decoded_samples) => {
                        let total = decoded_samples * ch as usize;
                        let mut ring = ring_writer.lock().unwrap();
                        for &s in &pcm_i16[..total] {
                            ring.push_back(s as f32 / 32768.0);
                        }
                        // Trim if too far ahead (prevent unbounded growth)
                        while ring.len() > ring_size * 2 {
                            ring.pop_front();
                        }
                    }
                    Err(e) => {
                        warn!("Opus decode error: {e}");
                    }
                }
            }
            info!("audio decode thread stopped");
        })
        .context("spawn audio decode thread")?;

    // cpal output stream: pulls from ring buffer
    let host = cpal::default_host();
    let device = host.default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;

    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let ring_reader = std::sync::Arc::clone(&ring);
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let mut ring = ring_reader.lock().unwrap();
            for sample in data.iter_mut() {
                *sample = ring.pop_front().unwrap_or(0.0);
            }
        },
        move |err| {
            warn!("audio output error: {err}");
        },
        None,
    ).context("build audio output stream")?;

    stream.play().context("start audio playback")?;

    // Keep stream alive by leaking it (it lives for the session duration).
    // The stream stops when the process exits.
    std::mem::forget(stream);

    info!(sample_rate, channels, "audio playback started");
    Ok(tx)
}
