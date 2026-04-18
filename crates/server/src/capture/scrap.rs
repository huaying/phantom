use anyhow::{Context, Result};
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use scrap::{Capturer, Display};
use std::time::Instant;

/// CPU-based screen capture using the `scrap` crate.
/// Uses DXGI (Windows), Core Graphics (macOS), X11 (Linux).
pub struct ScrapCapture {
    capturer: Option<Capturer>,
    width: u32,
    height: u32,
    display_index: usize,
}

impl ScrapCapture {
    /// Create a capturer for the primary display.
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        Self::with_display(0)
    }

    /// Create a capturer for a specific display by index.
    ///
    /// Index 0 is the primary display. Use `list_displays()` to enumerate.
    pub fn with_display(index: usize) -> Result<Self> {
        let displays = Display::all().context("failed to enumerate displays")?;
        if displays.is_empty() {
            anyhow::bail!("no displays found");
        }
        if index >= displays.len() {
            anyhow::bail!(
                "display index {index} out of range (found {} display{})",
                displays.len(),
                if displays.len() == 1 { "" } else { "s" }
            );
        }

        let display = displays
            .into_iter()
            .nth(index)
            .context("display disappeared during enumeration")?;
        let width = display.width() as u32;
        let height = display.height() as u32;
        let capturer = Capturer::new(display).context("failed to create capturer")?;
        tracing::info!(index, width, height, "ScrapCapture initialized");
        Ok(Self {
            capturer: Some(capturer),
            width,
            height,
            display_index: index,
        })
    }

    /// Recreate the capturer. Resets DXGI Desktop Duplication state so the
    /// next capture() call returns a frame even on a static desktop.
    pub fn reset(&mut self) -> Result<()> {
        let displays = Display::all().context("failed to enumerate displays")?;
        let index = self.display_index.min(displays.len().saturating_sub(1));
        let display = displays
            .into_iter()
            .nth(index)
            .context("display disappeared during reset")?;
        self.width = display.width() as u32;
        self.height = display.height() as u32;
        // Drop old capturer BEFORE creating new — only one DuplicateOutput per output.
        self.capturer = None;
        self.capturer = Some(Capturer::new(display).context("failed to recreate capturer")?);
        self.display_index = index;
        tracing::debug!(index, "ScrapCapture reset");
        Ok(())
    }

    /// List all available displays with their index and resolution.
    pub fn list_displays() -> Result<Vec<DisplayInfo>> {
        let displays = Display::all().context("failed to enumerate displays")?;
        Ok(displays
            .iter()
            .enumerate()
            .map(|(i, d)| DisplayInfo {
                index: i,
                width: d.width() as u32,
                height: d.height() as u32,
                is_primary: i == 0,
            })
            .collect())
    }
}

/// Information about an available display.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

impl std::fmt::Display for DisplayInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Display {}: {}x{}{}",
            self.index,
            self.width,
            self.height,
            if self.is_primary { " (primary)" } else { "" }
        )
    }
}

impl FrameCapture for ScrapCapture {
    fn capture(&mut self) -> Result<Option<Frame>> {
        let capturer = self.capturer.as_mut().context("capturer not initialized")?;
        match capturer.frame() {
            Ok(frame) => {
                // scrap returns BGRA on all platforms, but stride may differ
                // from width * 4 due to padding. Copy row by row.
                if self.height == 0 || self.width == 0 {
                    return Ok(None);
                }
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

    fn reset(&mut self) -> Result<()> {
        self.reset()
    }
}
