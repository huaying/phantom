//! StreamSource trait — the core abstraction for GPU streaming.
//!
//! Any application that can produce GPU framebuffers implements this trait
//! to get automatic NVENC encoding + network streaming.

use std::time::Instant;

/// A raw CUDA device pointer (u64 on Linux, usize on Windows).
pub type CUdeviceptr = u64;

/// Pixel format of the GPU buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPixelFormat {
    /// NV12 (Y plane + interleaved UV). Native NVENC input — zero conversion.
    Nv12,
    /// RGBA 8-bit per channel. Needs GPU color conversion to NV12.
    Rgba8,
    /// BGRA 8-bit per channel. Needs GPU color conversion to NV12.
    Bgra8,
}

/// A frame residing on the GPU (CUDA device memory).
pub struct GpuFrame {
    /// CUDA device pointer to the frame data.
    pub device_ptr: CUdeviceptr,
    /// Pitch in bytes (bytes per row, may include padding).
    pub pitch: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: GpuPixelFormat,
    /// Capture timestamp.
    pub timestamp: Instant,
}

/// A frame residing in CPU memory (fallback path).
pub struct CpuFrame {
    /// Raw pixel data.
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format (Rgba8 or Bgra8).
    pub format: GpuPixelFormat,
    /// Capture timestamp.
    pub timestamp: Instant,
}

/// Frame output from a StreamSource — either GPU or CPU.
pub enum StreamFrame {
    Gpu(GpuFrame),
    Cpu(CpuFrame),
}

/// Trait for any application that produces frames to be streamed.
///
/// Implement this to connect your GPU application to Phantom's
/// encoding and streaming pipeline.
///
/// # Examples
///
/// ```rust,ignore
/// use phantom_stream::{StreamSource, GpuFrame, GpuPixelFormat, StreamFrame};
/// use std::time::Instant;
///
/// struct ParticleRenderer {
///     framebuffer_ptr: u64,
///     width: u32,
///     height: u32,
/// }
///
/// impl StreamSource for ParticleRenderer {
///     fn resolution(&self) -> (u32, u32) {
///         (self.width, self.height)
///     }
///
///     fn next_frame(&mut self) -> anyhow::Result<Option<StreamFrame>> {
///         // ... render particles to framebuffer ...
///         Ok(Some(StreamFrame::Gpu(GpuFrame {
///             device_ptr: self.framebuffer_ptr,
///             pitch: self.width * 4,
///             width: self.width,
///             height: self.height,
///             format: GpuPixelFormat::Rgba8,
///             timestamp: Instant::now(),
///         })))
///     }
/// }
/// ```
pub trait StreamSource: Send {
    /// The output resolution (width, height).
    fn resolution(&self) -> (u32, u32);

    /// Produce the next frame. Returns `Ok(None)` if no new frame is ready.
    ///
    /// The pipeline calls this in a loop at the target FPS.
    /// For GPU frames, the device pointer must remain valid until the next call.
    fn next_frame(&mut self) -> anyhow::Result<Option<StreamFrame>>;

    /// Optional: called when a client connects. Can be used to start rendering.
    fn on_client_connect(&mut self) {}

    /// Optional: called when all clients disconnect. Can pause rendering to save GPU.
    fn on_client_disconnect(&mut self) {}
}
