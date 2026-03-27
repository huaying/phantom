use crate::decode::DecodedTile;
use anyhow::Result;

/// Trait for client-side display rendering.
///
/// Implementations:
/// - `MinifbDisplay`: simple CPU framebuffer window (via `minifb`)
/// - Future: `WgpuDisplay` (GPU-composited), `WebDisplay` (WebCodecs + Canvas)
pub trait Display {
    /// Initialize the display with the given resolution.
    fn init(&mut self, width: u32, height: u32) -> Result<()>;

    /// Apply decoded tiles to the internal framebuffer.
    fn update_tiles(&mut self, tiles: &[DecodedTile]) -> Result<()>;

    /// Present the framebuffer to the screen. Returns false if window was closed.
    fn present(&mut self) -> Result<bool>;
}
