//! Synthetic browser media source for idle-to-motion presentation checks.
//! See docs/testing-runbook.md. Never captures the desktop or injects input.

use anyhow::{Context, Result};
use clap::Parser;
use phantom_core::encode::{FrameEncoder, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::protocol::{AudioCodec, CursorShape, CursorState, Message, PROTOCOL_VERSION};
use phantom_core::transport::{MessageReceiver, MessageSender};
use phantom_server::encode::h264::OpenH264Encoder;
use phantom_server::transport::tcp::TcpServerTransport;
use phantom_server::transport::ws::WebServerTransport;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const AUDIO_SAMPLES: usize = 960;
static NEXT_CURSOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 9921)]
    port: u16,
    #[arg(long, default_value_t = 20)]
    idle_seconds: u64,
    #[arg(long)]
    no_audio: bool,
    /// Serve plaintext TCP on loopback for the native client instead of HTTPS.
    #[arg(long)]
    tcp: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    if args.tcp {
        let server = TcpServerTransport::bind(&format!("127.0.0.1:{}", args.port))?;
        loop {
            let (sender, receiver) = server.accept_tcp()?.split()?;
            if let Err(error) = serve(Box::new(sender), Box::new(receiver), &args) {
                tracing::info!(%error, "synthetic TCP session ended");
            }
        }
    }
    let udp_port = args.port.checked_add(2).context("port must be <= 65533")?;
    std::env::set_var("PHANTOM_HOST", "127.0.0.1");
    let server = WebServerTransport::start(args.port, args.port + 1, udp_port, None)?;
    println!(
        "Synthetic media: https://127.0.0.1:{}/?rtc (or ?wss)",
        args.port
    );
    loop {
        let (sender, receiver) = server.accept_any()?;
        if let Err(error) = serve(sender, receiver, &args) {
            tracing::info!(%error, "synthetic session ended");
        }
    }
}

fn serve(
    mut sender: Box<dyn MessageSender>,
    mut receiver: Box<dyn MessageReceiver>,
    args: &Args,
) -> Result<()> {
    // A fresh shape per connection detects missing subscriptions even when
    // the browser retains a previously rendered cursor in its cache.
    let cursor_id = NEXT_CURSOR_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel(32);
    std::thread::spawn(move || {
        while let Ok(message) = receiver.recv_msg() {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    let mut session_token = vec![0; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut session_token)
        .map_err(|_| anyhow::anyhow!("session token RNG failed"))?;
    sender.send_msg(&Message::Hello {
        width: WIDTH,
        height: HEIGHT,
        format: PixelFormat::Bgra8,
        protocol_version: PROTOCOL_VERSION,
        audio: !args.no_audio,
        video_codec: VideoCodec::H264,
        session_token,
    })?;
    let mut encoder = OpenH264Encoder::new(WIDTH, HEIGHT, 25.0, 1500)?;
    let opus = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )?;
    let start = Instant::now();
    let mut next_tick = start;
    let mut tick = 0_u64;
    let mut sequence = 0;
    let mut phase_before = "";
    let mut last_frame = synthetic_frame(Duration::ZERO, false)?;
    let mut pcm = vec![0_i16; AUDIO_SAMPLES * 2];
    let mut audio_packet = vec![0_u8; 4000];
    loop {
        let elapsed = start.elapsed();
        // Three seconds to warm up, a truly static interval, then twelve
        // seconds of motion. The cycle repeats while the browser is connected.
        let cycle = elapsed.as_secs() % (15 + args.idle_seconds);
        let phase = if cycle < 3 {
            "warmup"
        } else if cycle < 3 + args.idle_seconds {
            "idle"
        } else {
            "motion"
        };
        if phase != phase_before {
            tracing::info!(phase, elapsed_ms = elapsed.as_millis(), "synthetic phase");
            phase_before = phase;
        }
        let mut keyframe = sequence == 0;
        loop {
            match rx.try_recv() {
                Ok(Message::Ping) => sender.send_msg(&Message::Pong)?,
                Ok(Message::RequestKeyframe) => {
                    sender.send_msg(&Message::KeyframeFence)?;
                    keyframe = true;
                }
                Ok(Message::EnableCursorState { enabled: true }) => {
                    sender.send_msg(&Message::CursorUpdate(CursorState {
                        visible: true,
                        x: 100,
                        y: 100,
                        shape_id: cursor_id,
                    }))?;
                }
                Ok(Message::EnableCursorShape { enabled: true }) => {
                    sender.send_msg(&Message::CursorShape(CursorShape {
                        shape_id: cursor_id,
                        width: 8,
                        height: 8,
                        hotspot_x: 0,
                        hotspot_y: 0,
                        rgba: [255, 255, 255, 255].repeat(64),
                    }))?;
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("peer closed"),
            }
        }
        if keyframe || (phase != "idle" && tick.is_multiple_of(2)) {
            if keyframe {
                encoder.force_keyframe();
            }
            if phase != "idle" {
                last_frame = synthetic_frame(elapsed, phase == "motion")?;
            }
            let encoded = encoder.encode_frame(&last_frame)?;
            if !encoded.data.is_empty() {
                sequence += 1;
                sender.send_msg(&Message::VideoFrame {
                    sequence,
                    frame: Box::new(encoded),
                })?;
            }
        }
        if !args.no_audio {
            for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
                let t = (tick * AUDIO_SAMPLES as u64 + index as u64) as f64 / 48_000.0;
                let amplitude = (t * 440.0 * std::f64::consts::TAU).sin() * 1500.0;
                sample.fill(amplitude as i16);
            }
            let size = opus.encode(&pcm, &mut audio_packet)?;
            sender.send_msg(&Message::AudioFrame {
                codec: AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 2,
                data: audio_packet[..size].to_vec(),
            })?;
        }
        tick += 1;
        next_tick += Duration::from_millis(20);
        std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
    }
}

fn synthetic_frame(elapsed: Duration, motion: bool) -> Result<Frame> {
    let mut frame = Frame {
        width: WIDTH,
        height: HEIGHT,
        format: PixelFormat::Bgra8,
        data: vec![0; (WIDTH * HEIGHT * 4) as usize],
        timestamp: Instant::now(),
    };
    let wall_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u32;
    let marker_x = (elapsed.as_millis() / 4 % u128::from(WIDTH - 40)) as u32;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let pixel = &mut frame.data[((y * WIDTH + x) * 4) as usize..][..4];
            let color = if y < 40 {
                // 32 black/white cells encode low 32 bits of UNIX milliseconds,
                // most significant bit first. Sample cell centers to measure
                // rendered-frame age independently of RTP/RTCP counters.
                let value = if wall_ms & (1 << (31 - x / 20)) != 0 {
                    240
                } else {
                    16
                };
                [value, value, value, 255]
            } else if (marker_x..marker_x + 40).contains(&x) {
                [240, 240, 240, 255]
            } else if motion {
                [65, 130, 35, 255]
            } else {
                [120, 55, 35, 255]
            };
            pixel.copy_from_slice(&color);
        }
    }
    Ok(frame)
}
