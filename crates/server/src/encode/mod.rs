//! Encoders that run on the CPU. GPU-backed encoders (NVENC, NVDEC) live
//! in the phantom-gpu crate so they can be loaded via runtime dlopen
//! without forcing CUDA on every build.

pub mod h264;
