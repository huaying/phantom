//! Opus decode → cpal audio playback.
//!
//! Receives Opus-encoded audio chunks from the server and plays them through
//! the default audio output device using cpal.

use anyhow::{Context, Result};
use std::sync::mpsc;
use tracing::{info, warn};

/// Start the audio playback pipeline. Returns a sender that accepts raw Opus
/// packets. Drop the sender to stop playback.
pub fn start_playback(sample_rate: u32, channels: u8) -> Result<mpsc::SyncSender<Vec<u8>>> {
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
    // Size = 300ms target. We don't start playing until the ring has filled
    // to ~100ms so that brief network jitter at startup doesn't immediately
    // underrun (was: 200ms target with no priming, music skipped on every
    // ~100ms+ network gap).
    let ring_size = (sample_rate as usize) * (channels as usize) * 300 / 1000;
    let prime_samples = ring_size / 3; // ~100ms
    let ring = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<f32>::with_capacity(ring_size),
    ));
    let ring_writer = std::sync::Arc::clone(&ring);

    // Cross-thread state:
    //  - primed: false until ring fills to prime_samples; while !primed the
    //    cpal callback outputs silence so we don't pop the few samples that
    //    arrived early and immediately underrun.
    //  - underrun_samples: sample count we couldn't fulfil from the ring.
    //    Used by the monitor thread for periodic stats log.
    let primed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let underrun_samples = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // trim_count: how many times the decoder has had to drain the ring back
    // to prime_samples because a network burst pushed it past ring_size.
    // Each trim is roughly one audible click; spike here means jitter is
    // bigger than the buffer wants to absorb.
    let trim_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let primed_cb = std::sync::Arc::clone(&primed);
    let underrun_cb = std::sync::Arc::clone(&underrun_samples);
    let underrun_mon = std::sync::Arc::clone(&underrun_samples);
    let trim_mon = std::sync::Arc::clone(&trim_count);
    let ring_mon = std::sync::Arc::clone(&ring);

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
                let Ok(out) = audiopus::MutSignals::try_from(&mut pcm_i16[..]) else {
                    warn!("failed to create MutSignals buffer");
                    continue;
                };
                let packet = match audiopus::packet::Packet::try_from(opus_data.as_slice()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("invalid Opus packet: {e}");
                        continue;
                    }
                };
                match decoder.decode(
                    Some(packet),
                    out,
                    false, // no FEC
                ) {
                    Ok(decoded_samples) => {
                        let total = decoded_samples * ch as usize;
                        if let Ok(mut ring) = ring_writer.lock() {
                            for &s in &pcm_i16[..total] {
                                ring.push_back(s as f32 / 32768.0);
                            }
                            // Soft cap: when the ring stays above ring_size
                            // (300ms) the decoder shaves a tiny slice off
                            // the head each frame. Per-call drop is small
                            // enough to be inaudible (think very slight
                            // playback speed-up) but over many frames it
                            // pulls the running latency back toward the
                            // target instead of letting burst headroom
                            // permanently park in the buffer.
                            if ring.len() > ring_size {
                                // Drop ~5% of one Opus frame per call
                                // (~1ms at 48kHz stereo). Amortised drain.
                                let shave = (frame_samples * ch as usize) / 20;
                                for _ in 0..shave.min(ring.len() - ring_size) {
                                    ring.pop_front();
                                }
                                trim_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
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
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;

    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let ring_reader = std::sync::Arc::clone(&ring);
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let Ok(mut ring) = ring_reader.lock() else {
                    data.fill(0.0);
                    return;
                };
                // Cold start: stay silent until the ring has primed. Once
                // primed it stays primed for the lifetime of the stream;
                // sustained silence after that is real underrun.
                if !primed_cb.load(std::sync::atomic::Ordering::Relaxed) {
                    if ring.len() >= prime_samples {
                        primed_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        data.fill(0.0);
                        return;
                    }
                }
                let mut underrun = 0u64;
                for sample in data.iter_mut() {
                    match ring.pop_front() {
                        Some(s) => *sample = s,
                        None => {
                            *sample = 0.0;
                            underrun += 1;
                        }
                    }
                }
                if underrun > 0 {
                    underrun_cb.fetch_add(underrun, std::sync::atomic::Ordering::Relaxed);
                }
            },
            move |err| {
                warn!("audio output error: {err}");
            },
            None,
        )
        .context("build audio output stream")?;

    stream.play().context("start audio playback")?;

    // Keep stream alive by leaking it (it lives for the session duration).
    // The stream stops when the process exits.
    std::mem::forget(stream);

    // Monitor thread: every 5s log underrun rate + ring depth so we can
    // tell if jitter buffer is too small / network is dropping packets.
    // sample_rate * channels = bytes/ms when we divide by 1000; we report
    // underrun as ms-of-silence so it's easier to reason about than raw
    // sample counts.
    let samples_per_ms = (sample_rate as u64) * (channels as u64) / 1000;
    std::thread::Builder::new()
        .name("audio-monitor".into())
        .spawn(move || {
            let mut last_ur = 0u64;
            let mut last_tr = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let ur = underrun_mon.load(std::sync::atomic::Ordering::Relaxed);
                let tr = trim_mon.load(std::sync::atomic::Ordering::Relaxed);
                let dur = ur.saturating_sub(last_ur);
                let dtr = tr.saturating_sub(last_tr);
                last_ur = ur;
                last_tr = tr;
                let depth_ms = ring_mon
                    .lock()
                    .map(|r| r.len() as u64 / samples_per_ms.max(1))
                    .unwrap_or(0);
                if dur > 0 || dtr > 0 {
                    let underrun_ms = dur / samples_per_ms.max(1);
                    info!(
                        underrun_ms_5s = underrun_ms,
                        trims_5s = dtr,
                        ring_depth_ms = depth_ms,
                        "audio stats"
                    );
                } else {
                    tracing::debug!(ring_depth_ms = depth_ms, "audio stats");
                }
            }
        })
        .context("spawn audio monitor thread")?;

    info!(sample_rate, channels, "audio playback started");
    Ok(tx)
}
