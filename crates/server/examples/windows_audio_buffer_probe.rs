//! Compare WASAPI loopback buffer sizes without recording audio or changing a device.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This diagnostic requires Windows.");
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        arguments
            .iter()
            .all(|argument| matches!(argument.as_str(), "--tone" | "--timer-1ms")),
        "usage: windows_audio_buffer_probe [--tone] [--timer-1ms]"
    );
    let _timer = if arguments.iter().any(|argument| argument == "--timer-1ms") {
        Some(TimerPeriod::start()?)
    } else {
        None
    };
    let render = arguments
        .iter()
        .any(|argument| argument == "--tone")
        .then(|| std::thread::spawn(render_tone));
    let threads: Vec<_> = [(20, false), (100, false), (100, true)]
        .into_iter()
        .map(|(buffer_ms, events)| std::thread::spawn(move || probe(buffer_ms, events)))
        .collect();
    for thread in threads {
        thread.join().expect("capture probe thread panicked")?;
    }
    if let Some(render) = render {
        render.join().expect("render probe thread panicked")?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

#[cfg(target_os = "windows")]
struct TimerPeriod;

#[cfg(target_os = "windows")]
impl TimerPeriod {
    fn start() -> anyhow::Result<Self> {
        anyhow::ensure!(unsafe { timeBeginPeriod(1) } == 0, "timeBeginPeriod failed");
        println!("timer_period_ms=1");
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl Drop for TimerPeriod {
    fn drop(&mut self) {
        let result = unsafe { timeEndPeriod(1) };
        println!("timer_period_released={}", result == 0);
    }
}

/// Optional independent playback source with a deliberately large buffer.
/// Report whether the renderer was starved, alongside its device-clock rate.
#[cfg(target_os = "windows")]
fn render_tone() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let sample_rate = (*format).nSamplesPerSec;
        let channels = usize::from((*format).nChannels);
        let is_float = (*format).wFormatTag == 3
            || ((*format).wFormatTag == 0xfffe && (*format).cbSize >= 22 && {
                let subtype = (*format.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat;
                subtype == windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
            });
        if !is_float || (*format).wBitsPerSample != 32 || channels == 0 {
            CoTaskMemFree(Some(format.cast()));
            anyhow::bail!("tone diagnostic requires a 32-bit float mix format");
        }
        let initialized = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            2_000_000, // 200 ms, independent of the capture clients' buffer sizes
            0,
            format,
            None,
        );
        CoTaskMemFree(Some(format.cast()));
        initialized?;
        let buffer_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        let clock: IAudioClock = client.GetService()?;
        let frequency = clock.GetFrequency()?;
        let started = Instant::now();
        let mut last_report = started;
        let mut playing = false;
        let mut frame_index = 0_u64;
        let mut frames_written = 0_u64;
        let mut empty_polls = 0_u64;
        let mut minimum_padding = buffer_frames;
        let mut previous_position = 0;
        while started.elapsed() < Duration::from_secs(45) {
            let padding = client.GetCurrentPadding()?;
            if playing {
                minimum_padding = minimum_padding.min(padding);
                empty_polls += u64::from(padding == 0);
            }
            let count = buffer_frames - padding;
            if count > 0 {
                let data = render.GetBuffer(count)?;
                let samples =
                    std::slice::from_raw_parts_mut(data.cast::<f32>(), count as usize * channels);
                for frame in samples.chunks_exact_mut(channels) {
                    let phase =
                        frame_index as f64 * 440.0 * std::f64::consts::TAU / f64::from(sample_rate);
                    frame.fill((phase.sin() * 0.02) as f32);
                    frame_index += 1;
                }
                render.ReleaseBuffer(count, 0)?;
                frames_written += u64::from(count);
            }
            if !playing {
                client.Start()?;
                playing = true;
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                let mut position = 0;
                clock.GetPosition(&mut position, None)?;
                println!(
                    "render buffer_frames={buffer_frames} rate={sample_rate} elapsed_ms={} frames_written={frames_written} min_padding={minimum_padding} empty_polls={empty_polls} clock_delta={} clock_frequency={frequency}",
                    last_report.elapsed().as_millis(),
                    position.saturating_sub(previous_position)
                );
                previous_position = position;
                frames_written = 0;
                empty_polls = 0;
                minimum_padding = buffer_frames;
                last_report = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        client.Stop()?;
        drop(clock);
        drop(render);
        drop(client);
        drop(device);
        drop(enumerator);
        CoUninitialize();
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn probe(buffer_ms: i64, events: bool) -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let sample_rate = (*format).nSamplesPerSec;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | if events {
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                } else {
                    0
                },
            buffer_ms * 10_000,
            0,
            format,
            None,
        )?;
        CoTaskMemFree(Some(format.cast()));
        let buffer_frames = client.GetBufferSize()?;
        let capture: IAudioCaptureClient = client.GetService()?;
        let event = if events {
            let event = CreateEventW(None, false, false, None)?;
            client.SetEventHandle(event)?;
            Some(event)
        } else {
            None
        };
        client.Start()?;
        let started = Instant::now();
        let mut last_report = started;
        let mut frames = 0_u64;
        let mut packets = 0_u64;
        let mut discontinuities = 0_u64;
        let mut silent = 0_u64;
        let mut missing_frames = 0_u64;
        let mut expected_position = None;
        while started.elapsed() < Duration::from_secs(45) {
            if let Some(event) = event {
                WaitForSingleObject(event, 50);
            } else if capture.GetNextPacketSize()? == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            loop {
                let mut data = std::ptr::null_mut();
                let mut count = 0;
                let mut flags = 0;
                let mut position = 0;
                capture.GetBuffer(&mut data, &mut count, &mut flags, Some(&mut position), None)?;
                if count == 0 {
                    break;
                }
                // Only inspect metadata. Never read or save the audio samples.
                capture.ReleaseBuffer(count)?;
                frames += u64::from(count);
                packets += 1;
                discontinuities += u64::from(flags & 1 != 0);
                silent += u64::from(flags & 2 != 0);
                if flags & 4 == 0 {
                    if let Some(expected) = expected_position {
                        missing_frames += position.saturating_sub(expected);
                    }
                    expected_position = Some(position + u64::from(count));
                } else {
                    expected_position = None;
                }
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                println!(
                    "buffer_ms={buffer_ms} events={events} buffer_frames={buffer_frames} rate={sample_rate} elapsed_ms={} frames={frames} packets={packets} discontinuities={discontinuities} silent={silent} missing_frames={missing_frames}",
                    last_report.elapsed().as_millis()
                );
                last_report = Instant::now();
                frames = 0;
                packets = 0;
                discontinuities = 0;
                silent = 0;
                missing_frames = 0;
            }
        }
        client.Stop()?;
        if let Some(event) = event {
            CloseHandle(event)?;
        }
        drop(capture);
        drop(client);
        drop(device);
        drop(enumerator);
        CoUninitialize();
    }
    Ok(())
}
