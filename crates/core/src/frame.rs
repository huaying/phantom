use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
}

impl PixelFormat {
    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Bgra8 | Self::Rgba8 => 4,
        }
    }
}

/// A raw frame captured from the screen.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub timestamp: Instant,
}

impl Frame {
    pub fn stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_pixel()
    }

    pub fn total_bytes(&self) -> usize {
        self.stride() * self.height as usize
    }
}
