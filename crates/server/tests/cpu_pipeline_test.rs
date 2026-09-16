//! CPU capture must deliver small updates and recover after a skipped frame.
use anyhow::Result;
use phantom_core::capture::FrameCapture;
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_core::frame::{Frame, PixelFormat};
use phantom_core::tile::TileDiffer;
use phantom_server::pipeline::{CpuPipeline, Pipeline, TickCtx};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct Frames(VecDeque<Frame>);

impl FrameCapture for Frames {
    fn capture(&mut self) -> Result<Option<Frame>> {
        Ok(self.0.pop_front())
    }

    fn resolution(&self) -> (u32, u32) {
        (128, 128)
    }
}

#[derive(Default)]
struct Encoder(bool);

impl FrameEncoder for Encoder {
    fn encode_frame(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        Ok(EncodedFrame {
            codec: VideoCodec::H264,
            data: frame.data.clone(),
            is_keyframe: std::mem::take(&mut self.0),
        })
    }

    fn force_keyframe(&mut self) {
        self.0 = true;
    }
}

fn frame(changed_pixel: Option<(usize, usize)>) -> Frame {
    let mut data = vec![0; 128 * 128 * 4];
    if let Some((x, y)) = changed_pixel {
        data[(y * 128 + x) * 4] = 255;
    }
    Frame {
        width: 128,
        height: 128,
        format: PixelFormat::Bgra8,
        data,
        timestamp: Instant::now(),
    }
}

fn tick(needs_keyframe: bool) -> TickCtx {
    TickCtx {
        had_input: false,
        needs_keyframe,
    }
}

#[test]
fn small_idle_update_is_encoded_without_input() {
    let mut capture = Frames(VecDeque::from([frame(None), frame(Some((37, 29)))]));
    let mut encoder = Encoder::default();
    let mut differ = TileDiffer::new();
    let mut pipeline = CpuPipeline::new(
        &mut capture,
        &mut encoder,
        &mut differ,
        Duration::from_millis(33),
    )
    .unwrap();
    assert!(pipeline.tick(tick(false)).unwrap().is_some());
    let update = pipeline.tick(tick(false)).unwrap().expect("small update");
    assert_eq!(update.encoded.data[(29 * 128 + 37) * 4], 255);
}

#[test]
fn static_desktop_honors_keyframe_request() {
    let mut capture = Frames(VecDeque::from([frame(None), frame(None), frame(None)]));
    let mut encoder = Encoder::default();
    let mut differ = TileDiffer::new();
    let mut pipeline = CpuPipeline::new(
        &mut capture,
        &mut encoder,
        &mut differ,
        Duration::from_millis(33),
    )
    .unwrap();
    assert!(pipeline.tick(tick(false)).unwrap().is_some());
    assert!(pipeline.tick(tick(false)).unwrap().is_none());
    let recovery = pipeline.tick(tick(true)).unwrap().expect("recovery IDR");
    assert!(recovery.encoded.is_keyframe);
}

#[test]
fn congestion_skip_preserves_pending_update() {
    let mut capture = Frames(VecDeque::from([
        frame(None),
        frame(Some((64, 0))),
        frame(Some((64, 0))),
        frame(Some((64, 0))),
    ]));
    let mut encoder = Encoder::default();
    let mut differ = TileDiffer::new();
    let mut pipeline = CpuPipeline::new(
        &mut capture,
        &mut encoder,
        &mut differ,
        Duration::from_millis(33),
    )
    .unwrap();
    assert!(pipeline.tick(tick(false)).unwrap().is_some());
    for _ in 0..4 {
        pipeline
            .congestion_mut()
            .unwrap()
            .on_frame_sent(Duration::from_millis(100));
    }
    assert!(pipeline.tick(tick(false)).unwrap().is_none());
    let update = pipeline.tick(tick(false)).unwrap().expect("pending update");
    assert_eq!(update.encoded.data[64 * 4], 255);
    assert!(pipeline.tick(tick(false)).unwrap().is_none());
}
