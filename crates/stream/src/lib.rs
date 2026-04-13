//! # phantom-stream — GPU streaming library
//!
//! Stream any GPU framebuffer over the network with hardware encoding.
//!
//! ```rust,ignore
//! use phantom_stream::{StreamSource, GpuFrame, StreamServer};
//!
//! struct MyRenderer { /* ... */ }
//!
//! impl StreamSource for MyRenderer {
//!     fn resolution(&self) -> (u32, u32) { (1920, 1080) }
//!     fn next_frame(&mut self) -> anyhow::Result<Option<GpuFrame>> {
//!         // render to GPU buffer, return pointer
//!         Ok(Some(GpuFrame { /* ... */ }))
//!     }
//! }
//!
//! let server = StreamServer::new(my_renderer, 8080)?;
//! server.run(); // blocks, streams frames to connected browsers
//! ```

mod source;
mod pipeline;
mod server;

pub use source::{StreamSource, GpuFrame, GpuPixelFormat, CpuFrame, StreamFrame};
pub use pipeline::StreamPipeline;
pub use server::StreamServer;
