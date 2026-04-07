//! GPU-accelerated capture and encoding via NVIDIA NVFBC + NVENC.
//!
//! All NVIDIA libraries are loaded at runtime via dlopen — no build-time
//! CUDA dependency. Falls back gracefully if libraries are unavailable.

pub(crate) mod dl;
pub mod sys;

pub mod cuda;
pub mod nvenc;
pub mod probe;
#[cfg(target_os = "linux")]
pub mod nvfbc;
#[cfg(target_os = "windows")]
pub mod dxgi;
#[cfg(target_os = "windows")]
pub mod dxgi_nvenc;
