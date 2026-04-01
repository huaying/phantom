use crate::frame::Frame;
use anyhow::Result;

/// Trait for screen frame capture.
///
/// Implementations:
/// - `ScrapCapture`: CPU-based, cross-platform (via `scrap` crate)
/// - Future: NVFBC (zero-copy GPU capture), DMA-BUF/KMS (Linux)
pub trait FrameCapture {
    /// Capture a single frame. Returns `Ok(None)` if no new frame is available yet.
    fn capture(&mut self) -> Result<Option<Frame>>;

    /// The current capture resolution.
    fn resolution(&self) -> (u32, u32);

    /// Reset the capturer for a new session. On Windows DXGI, this recreates
    /// the Desktop Duplication output so capture() returns a fresh frame.
    fn reset(&mut self) -> Result<()> {
        Ok(()) // default no-op
    }
}
