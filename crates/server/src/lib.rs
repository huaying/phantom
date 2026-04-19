//! phantom-server library surface.
//!
//! The crate is split into a library (this file) + a binary (`main.rs`). The
//! library exposes the modules that make up the streaming stack so integration
//! tests can drive them directly, and so external crates can (eventually,
//! once we finish task #23) embed phantom's streaming pipeline.
//!
//! The binary is just a thin CLI wrapper on top — it wires flags to library
//! calls.
//!
//! Only modules that are useful to test or embed are re-exported. Module
//! visibility (`pub fn` vs private) is the source of truth for "what external
//! callers can touch"; appearing in this list just means the module exists.

#[cfg(feature = "audio")]
pub mod audio;
pub mod capture;
#[cfg(target_os = "windows")]
pub mod display_ccd;
pub mod doorbell;
pub mod encode;
pub mod file_transfer;
pub mod input_injector;
#[cfg(target_os = "linux")]
pub mod input_uinput;
pub mod ipc_pipe;
#[cfg(target_os = "windows")]
pub mod service_win;
pub mod session;
pub mod transport;
