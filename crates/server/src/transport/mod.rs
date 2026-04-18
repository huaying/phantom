//! Network transports. All implement `MessageSender` / `MessageReceiver`
//! from phantom-core so the session loop is transport-agnostic.
//!
//! - `tcp` — plain or ChaCha20-Poly1305 over TCP
//! - `quic` — quinn (UDP), self-signed TLS, native client only
//! - `ws` — HTTPS + WebSocket upgrade on the same port; serves the
//!   embedded WASM web client too
//! - `webrtc` — str0m DataChannel (feature `webrtc`), browser path

pub mod quic;
pub mod tcp;
pub mod ws;

#[cfg(feature = "webrtc")]
pub mod webrtc;
