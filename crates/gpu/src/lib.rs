//! GPU-accelerated capture and encoding via NVIDIA NVFBC + NVENC.
//!
//! All NVIDIA libraries are loaded at runtime via dlopen — no build-time
//! CUDA dependency. Falls back gracefully if libraries are unavailable.

mod dl;
pub mod sys;

pub mod cuda;
pub mod nvenc;
#[cfg(target_os = "linux")]
pub mod nvfbc;
