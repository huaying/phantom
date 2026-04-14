//! Encoding pipeline: StreamSource → NVENC → encoded frames.

use crate::source::{GpuPixelFormat, StreamFrame, StreamSource};
use anyhow::{Context, Result};
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_gpu::cuda::CudaLib;
use phantom_gpu::nvenc::NvencEncoder;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Configuration for the streaming pipeline.
pub struct StreamConfig {
    /// Target frames per second.
    pub fps: u32,
    /// Initial bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Video codec.
    pub codec: VideoCodec,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 5000,
            codec: VideoCodec::H264,
        }
    }
}

/// The encoding pipeline.
pub struct StreamPipeline {
    config: StreamConfig,
    encoder: Option<NvencEncoder>,
    cuda: Option<Arc<CudaLib>>,
    frame_interval: Duration,
    last_frame_time: Instant,
    frame_count: u64,
}

impl StreamPipeline {
    pub fn new(config: StreamConfig) -> Self {
        let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
        Self {
            config,
            encoder: None,
            cuda: None,
            frame_interval,
            last_frame_time: Instant::now() - Duration::from_secs(1),
            frame_count: 0,
        }
    }

    /// Process one frame from the source. Returns encoded frame if available.
    pub fn process_frame(&mut self, source: &mut dyn StreamSource) -> Result<Option<EncodedFrame>> {
        let now = Instant::now();
        if now.duration_since(self.last_frame_time) < self.frame_interval {
            return Ok(None);
        }

        let frame = match source.next_frame()? {
            Some(f) => f,
            None => return Ok(None),
        };

        // Lazy init encoder
        if self.encoder.is_none() {
            let (w, h) = source.resolution();
            tracing::info!(width = w, height = h, codec = ?self.config.codec,
                bitrate = self.config.bitrate_kbps, "initializing NVENC encoder");

            let cuda = Arc::new(CudaLib::load().context("failed to load CUDA")?);
            let encoder = NvencEncoder::new(
                Arc::clone(&cuda),
                0, // device ordinal
                w,
                h,
                self.config.fps,
                self.config.bitrate_kbps,
                self.config.codec,
            )
            .context("failed to initialize NVENC encoder")?;

            self.cuda = Some(cuda);
            self.encoder = Some(encoder);
        }

        let encoder = self.encoder.as_mut().unwrap();

        let encoded = match frame {
            StreamFrame::Gpu(gpu_frame) => {
                match gpu_frame.format {
                    GpuPixelFormat::Nv12 => {
                        // Zero-copy: GPU NV12 → NVENC directly
                        encoder.encode_device_nv12(gpu_frame.device_ptr, gpu_frame.pitch)?
                    }
                    GpuPixelFormat::Rgba8 | GpuPixelFormat::Bgra8 => {
                        // TODO: GPU RGBA→NV12 conversion kernel
                        // Placeholder: CPU fallback
                        let cpu_frame = phantom_core::frame::Frame {
                            width: gpu_frame.width,
                            height: gpu_frame.height,
                            format: match gpu_frame.format {
                                GpuPixelFormat::Rgba8 => phantom_core::frame::PixelFormat::Rgba8,
                                _ => phantom_core::frame::PixelFormat::Bgra8,
                            },
                            data: vec![128u8; (gpu_frame.width * gpu_frame.height * 4) as usize],
                            timestamp: gpu_frame.timestamp,
                        };
                        encoder.encode_frame(&cpu_frame)?
                    }
                }
            }
            StreamFrame::Cpu(cpu_frame) => {
                let core_frame = phantom_core::frame::Frame {
                    width: cpu_frame.width,
                    height: cpu_frame.height,
                    format: match cpu_frame.format {
                        GpuPixelFormat::Rgba8 => phantom_core::frame::PixelFormat::Rgba8,
                        _ => phantom_core::frame::PixelFormat::Bgra8,
                    },
                    data: cpu_frame.data,
                    timestamp: cpu_frame.timestamp,
                };
                encoder.encode_frame(&core_frame)?
            }
        };

        self.last_frame_time = now;
        self.frame_count += 1;

        Ok(Some(encoded))
    }

    pub fn force_keyframe(&mut self) {
        if let Some(ref mut enc) = self.encoder {
            enc.force_keyframe();
        }
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        if let Some(ref mut enc) = self.encoder {
            enc.set_bitrate_kbps(kbps)?;
        }
        self.config.bitrate_kbps = kbps;
        Ok(())
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}
