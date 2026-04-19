//! Core library for the Phantom remote desktop system.
//!
//! This crate provides the shared protocol, codec, transport, and utility
//! types used by both the server and client. It is designed to be
//! platform-independent (no GPU, no windowing) so it can also compile
//! to WebAssembly for the browser client.
//!
//! # Module overview
//!
//! - [`protocol`] — Wire protocol (messages, serialization, versioning)
//! - [`transport`] — Trait abstractions for message send/receive (TCP, QUIC, etc.)
//! - [`color`] — BGRA ↔ YUV color space conversions with SIMD acceleration
//! - [`encode`] — Frame encoding trait and codec support
//! - [`tile`] — Tile-based dirty region detection (gates H.264 encode when idle)
//! - [`capture`] — Frame capture trait (implemented by platform-specific crates)
//! - [`input`] — Input event types (keyboard, mouse, scroll)
//! - [`clipboard`] — Clipboard synchronization with echo suppression
//! - [`file_transfer`] — Bidirectional file transfer state machine
//! - [`crypto`] — AES-256-GCM encrypted transport layer (feature-gated)

pub mod capture;
pub mod clipboard;
pub mod color;
#[cfg(feature = "crypto")]
pub mod crypto;
pub mod encode;
pub mod file_transfer;
pub mod frame;
pub mod input;
pub mod protocol;
pub mod stun;
pub mod tile;
pub mod transport;
