# Phantom Remote Desktop

A high-performance, open-source remote desktop built in Rust. Low latency, browser and native access, single binary deployment.

## Features

- **H.264 streaming** with periodic keyframes and lossless refinement after 2s idle
- **GPU acceleration** — NVENC encoding + DXGI zero-copy capture (Windows), NVFBC→NVENC (Linux)
- **Web client via WebSocket** — connect from any browser, zero install, WebCodecs H.264 decode
- **WebRTC DataChannel** (optional, `--features webrtc`) — for future NAT traversal
- **Native client** — winit + softbuffer with local cursor rendering
- **QUIC/UDP transport** — for native client, no head-of-line blocking
- **Encrypted by default** — ChaCha20-Poly1305 (TCP) or TLS (QUIC) or DTLS (WebRTC)
- **Clipboard sync** — bidirectional, with Ctrl+V paste injection
- **Auto-reconnect** — exponential backoff (native client)
- **Windows + Linux** — DXGI (Windows) / X11 (Linux) capture, auto-start support

## Quick Start

### Web Client (recommended)

```bash
# Server with web access
cargo run --release -p phantom-server -- --transport web --no-encrypt

# Open in browser (Chrome/Edge)
# → https://<server-ip>:9900
```

### GPU-Accelerated (Windows with NVIDIA GPU)

```bash
# DXGI→NVENC zero-copy (30-47fps at 1080p)
cargo run --release -p phantom-server -- --transport web --no-encrypt --capture dxgi --encoder nvenc --fps 60
```

### Native Client

```bash
# Server
cargo run --release -p phantom-server
# → prints: --key <hex>

# Client
cargo run --release -p phantom-client -- -c <server-ip>:9900 --key <hex>
```

### Docker (test environment with XFCE desktop)

```bash
docker build -t phantom .
docker run --rm -p 9900:9900 -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open https://127.0.0.1:9900
```

## Architecture

```
Native Client                        Server                         Web Client (Browser)
┌──────────────┐   QUIC/TCP   ┌──────────────────┐    WSS        ┌──────────────┐
│ OpenH264     │◄════════════╗│ Screen Capture    │╗══(TCP/TLS)═►│ WASM client  │
│ winit render │             ║│ H.264 Encode      │║             │ WebCodecs    │
│ Local cursor │             ║│ (OpenH264/NVENC)  │║             │ Canvas       │
│ OS key repeat│             ║│                    │║             │ Input capture│
│              │═════════════╝│ enigo inject      │╝═════════════│              │
│ Input capture│──────────────│                    │──────────────│ Keyboard/    │
│              │              │                    │              │ Mouse events │
└──────────────┘              └──────────────────┘              └──────────────┘
```

## Server Options

```
--listen <addr>              Listen address (default: 0.0.0.0:9900)
--transport <tcp|quic|web>   Transport protocol (default: tcp)
--fps <n>                    Target FPS (default: 30)
--bitrate <kbps>             H.264 bitrate (default: 5000)
--quality-delay-ms <ms>      Lossless update delay (default: 2000)
--encoder <openh264|nvenc>   Video encoder (default: openh264)
--capture <scrap|dxgi|nvfbc> Screen capture (default: scrap)
--key <hex>                  Encryption key (auto-generated if omitted)
--no-encrypt                 Disable encryption
--install / --uninstall      Auto-start (Windows: schtasks, Linux: systemd)
```

Environment variables:
- `PHANTOM_HOST` — IP for WebRTC ICE candidate (default: auto-detect)

## Performance

| Configuration | FPS (1080p) | Notes |
|--------------|-------------|-------|
| OpenH264 CPU (scrap) | 6-8 | Any machine, fallback |
| NVENC GPU (scrap) | 17-18 | NVIDIA GPU, CPU capture |
| DXGI→NVENC zero-copy | 30-47 | Windows + NVIDIA, all GPU |
| NVFBC→NVENC zero-copy | ~60+ | Linux + NVIDIA (X11) |

## Building

```bash
# Prerequisites
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build (WSS web transport, default)
cargo build --release

# Build with WebRTC support (adds str0m dependency)
cargo build --release --features webrtc

# Build WASM web client (pre-built pkg checked into repo)
wasm-pack build crates/web --target web --no-typescript

# Run tests
cargo test
```

## Project Structure

```
phantom/
├── crates/
│   ├── core/      Traits, protocol, frame, input, clipboard, color, crypto
│   ├── server/    Capture, encode, input inject, TCP/QUIC/WSS transports
│   ├── client/    Decode, winit display, input capture, reconnect
│   ├── web/       WASM client (WebCodecs, Canvas, WebSocket/WebRTC)
│   ├── gpu/       NVENC, NVFBC (Linux), DXGI capture (Windows), CUDA
│   └── bench/     Encoder benchmark (OpenH264 vs NVENC)
├── Dockerfile     XFCE desktop test environment
├── CLAUDE.md      Developer guide
└── DESIGN.md      Design document
```

## Roadmap

See [CLAUDE.md](CLAUDE.md) for the full roadmap. Key next steps:

- **Web client auto-reconnect** — handle WS disconnects gracefully
- **Audio forwarding** — PulseAudio capture → Opus → browser playback
- **Hardware probe** — auto-detect best encoder/capture at startup
- **WAN testing** — verify latency over real networks

## License

MIT
