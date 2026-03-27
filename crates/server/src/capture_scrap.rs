use anyhow::{Context, Result};
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use scrap::{Capturer, Display};
use std::time::Instant;

/// CPU-based screen capture using the `scrap` crate.
/// Uses DXGI (Windows), Core Graphics (macOS), X11 (Linux).
pub struct ScrapCapture {
    capturer: Capturer,
    width: u32,
    height: u32,
}

impl ScrapCapture {
    pub fn new() -> Result<Self> {
        let display = Display::primary().context("failed to get primary display")?;
        let width = display.width() as u32;
        let height = display.height() as u32;
        let capturer = Capturer::new(display).context("failed to create capturer")?;
        tracing::info!(width, height, "ScrapCapture initialized");
        Ok(Self { capturer, width, height })
    }
}

impl FrameCapture for ScrapCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        match self.capturer.frame() {
            Ok(frame) => {
                // scrap returns BGRA on all platforms, but stride may differ
                // from width * 4 due to padding. Copy row by row.
                let stride = frame.len() / self.height as usize;
                let bpp = 4;
                let expected_stride = self.width as usize * bpp;

                let data = if stride == expected_stride {
                    frame.to_vec()
                } else {
                    let mut data = Vec::with_capacity(expected_stride * self.height as usize);
                    for y in 0..self.height as usize {
                        let row_start = y * stride;
                        data.extend_from_slice(&frame[row_start..row_start + expected_stride]);
                    }
                    data
                };

                Ok(Some(Frame {
                    width: self.width,
                    height: self.height,
                    format: PixelFormat::Bgra8,
                    data,
                    timestamp: Instant::now(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}
