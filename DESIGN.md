# Phantom Remote Desktop — Design Document

> **Note:** This is a high-level design overview. For detailed implementation notes, pitfalls, and current architecture, see [CLAUDE.md](CLAUDE.md).

## Vision

A high-performance, open-source remote desktop built in Rust. Target: Parsec-class latency with pixel-perfect text quality, single binary deployment, browser + native access.

## Current Architecture

### Encoding Pipeline

```
Screen Capture → H.264 Encode → Send to Client → WebCodecs/OpenH264 Decode → Display
```

All screen changes are encoded as full H.264 frames. After 2 seconds of inactivity, a lossless zstd update is sent for pixel-perfect text.

**Capture backends:**
- `scrap` — CPU-based, cross-platform (DXGI on Windows, X11 on Linux)
- `dxgi` — GPU-resident D3D11 texture, Windows zero-copy (→ NVENC)
- `nvfbc` — NVIDIA FrameBuffer Capture, Linux zero-copy (→ NVENC)

**Encoder backends:**
- `openh264` — CPU H.264, Baseline profile, works everywhere
- `nvenc` — NVIDIA GPU H.264, runtime dlopen, no build-time CUDA dep

### Transport Layer

All transports implement `Box<dyn MessageSender>` + `Box<dyn MessageReceiver>`. Session loop is transport-agnostic.

| Transport | Use Case | Protocol |
|-----------|----------|----------|
| WSS (default web) | Browser client | WebSocket over TLS, same HTTPS port, HTTP/1.1 keep-alive |
| WebRTC DataChannel | Future NAT traversal | SCTP/DTLS/UDP, `--features webrtc` |
| TCP | Native client (LAN) | Raw TCP, optional ChaCha20 encryption |
| QUIC | Native client (WAN) | UDP, built-in TLS, no HOL blocking |

The HTTPS server uses HTTP/1.1 keep-alive to reuse TLS connections across multiple requests (e.g., loading index.html + .js + .wasm in one connection). A bounded connection pool (16 max threads) prevents thread explosion under load.

### Web Client

WASM module compiled from `crates/web/`, embedded in server binary via `include_bytes!`.
- Default: WebSocket (WSS on same HTTPS port)
- Optional: WebRTC DataChannel (`?rtc` URL param, requires `--features webrtc` build)
- H.264 decode via WebCodecs `VideoDecoder` (requires secure context → HTTPS)
- Renders to Canvas, captures mouse/keyboard/clipboard input

### GPU Zero-Copy Pipelines

**Linux (NVFBC → NVENC):**
```
NVFBC grab → CUdeviceptr (NV12, GPU) → NVENC encode → H.264 bytes (~4ms at 1080p)
```

**Windows (DXGI → NVENC):**
```
DXGI AcquireNextFrame → ID3D11Texture2D (BGRA, GPU) → CopyResource → NVENC encode → H.264 bytes (~3ms at 1080p)
```

Both paths: zero CPU readback, zero CPU color conversion, zero GPU upload.

## Performance

| Configuration | FPS (1080p) | Platform |
|--------------|-------------|----------|
| OpenH264 + scrap (CPU) | 6-8 | Any |
| NVENC + scrap (GPU encode, CPU capture) | 17-18 | Windows/Linux + NVIDIA |
| DXGI → NVENC (zero-copy) | 30-47 | Windows + NVIDIA |
| NVFBC → NVENC (zero-copy) | 60+ | Linux + NVIDIA |

## Key Design Decisions

1. **No GStreamer** — direct function calls, zero pipeline overhead
2. **WSS default over WebRTC** — simpler, more reliable, no SCTP message size issues
3. **WebRTC optional** — only needed for NAT traversal (behind feature flag)
4. **str0m sans-IO WebRTC** — pure Rust, no tokio dependency for WebRTC path
5. **Runtime dlopen for GPU** — compiles on any machine, GPU optional
6. **Self-signed HTTPS** — enables WebCodecs on non-localhost (rcgen)
7. **Periodic keyframes (2s)** — recovers from decode errors, supports reconnect
8. **Encoder recreation per session** — NVENC only outputs SPS/PPS on fresh encoder

## Roadmap

See [CLAUDE.md](CLAUDE.md) for the detailed roadmap with priorities.

## License

MIT
