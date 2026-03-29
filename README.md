# Phantom Remote Desktop

A high-performance, open-source remote desktop built in Rust. Pixel-perfect text, low latency, browser and native access, single binary deployment.

## Features

- **H.264 streaming** with two-phase rendering (lossy during motion → pixel-perfect when static)
- **Smart encoding** — small changes use lightweight tiles, large changes use H.264 (90% CPU savings)
- **Web client** — connect from any browser, zero install (WASM, 207KB)
- **Native client** — winit + softbuffer with local cursor rendering
- **QUIC/UDP transport** — no head-of-line blocking on WAN
- **Encrypted by default** — ChaCha20-Poly1305 (TCP) or TLS (QUIC)
- **Clipboard sync** — bidirectional, with Ctrl+V paste injection
- **Auto-reconnect** — exponential backoff, window persists

## Quick Start

### Native Client

```bash
# Server (Linux/Windows)
cargo run --release -p phantom-server
# → prints: --key <hex>

# Client (any OS)
cargo run --release -p phantom-client -- -c <server-ip>:9900 --key <hex>
```

### Web Client

```bash
# Server with web access
cargo run --release -p phantom-server -- --transport web --no-encrypt

# Open in browser
# → http://<server-ip>:9900
```

### Docker (test environment with XFCE desktop)

```bash
docker build -t phantom .

# Native client mode
docker run --rm -p 9900:9900 phantom server
cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900

# Web client mode
docker run --rm -p 9900:9900 -p 9901:9901 phantom server-web
# → open http://127.0.0.1:9900
```

## Architecture

```
Native Client                        Server                         Web Client
┌──────────────┐   QUIC/TCP   ┌──────────────────┐   WebSocket   ┌──────────────┐
│ OpenH264     │◄════════════╗│ Screen Capture    │╗════════════►│ WASM (207KB) │
│ winit render │             ║│ Smart Encode:     │║             │ WebCodecs    │
│ Local cursor │             ║│  <10% → zstd tiles│║             │ Canvas       │
│ OS key repeat│             ║│  ≥10% → H.264     │║             │ JS input     │
│              │═════════════╝│  static → lossless │╝═════════════│              │
│ Input capture│──────────────│ enigo inject      │──────────────│ Input capture│
└──────────────┘              └──────────────────┘              └──────────────┘
```

## Server Options

```
--listen <addr>         Listen address (default: 0.0.0.0:9900)
--transport <tcp|quic|web>  Transport protocol (default: tcp)
--fps <n>               Target FPS (default: 30)
--bitrate <kbps>        H.264 bitrate (default: 5000)
--quality-delay-ms <ms> Lossless update delay (default: 2000)
--encoder <name>        Video encoder (default: openh264)
--key <hex>             Encryption key (auto-generated if omitted)
--no-encrypt            Disable encryption
```

## How It Works

### Two-Phase Rendering
1. **Motion** → H.264 video stream (lossy, low latency)
2. **Static 2s** → zstd lossless tile update (pixel-perfect text and UI)

No other open-source remote desktop has this. Text is always crisp after motion stops.

### Smart Encoding
- Dirty area < 10% → send only changed tiles with zstd (0.1ms CPU)
- Dirty area ≥ 10% → encode full frame with H.264 (15ms CPU)
- Mouse cursor hidden on server → mouse movement costs 0 CPU

Result: 2-core cloud VM uses ~3% CPU for typical office work.

### Web Client (Rust → WASM)
The web client shares code with the server via `phantom-core`:
- Protocol parsing, input types, clipboard logic compiled to WASM
- H.264 decoded by browser's WebCodecs API (GPU hardware accelerated)
- Tile updates decompressed by ruzstd in WASM → Canvas putImageData

## Project Structure

```
phantom/
├── crates/
│   ├── core/      Traits, protocol, tile differ, color, crypto, clipboard
│   ├── server/    Capture, encode, input inject, TCP/QUIC/WS transports
│   ├── client/    Decode, winit display, input capture, reconnect
│   └── web/       WASM client (WebCodecs, Canvas, input)
├── Dockerfile     XFCE desktop test environment
└── DESIGN.md      Full design document + roadmap
```

## Building

```bash
# Prerequisites
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install wasm-pack  # for web client

# Build native
cargo build --release

# Build web client WASM
wasm-pack build crates/web --target web

# Run tests
cargo test
```

## Roadmap

See [DESIGN.md](DESIGN.md) for the full roadmap. Key next steps:

- **WebRTC DataChannel** — upgrade web client from WebSocket to UDP (20-50ms vs 80-150ms)
- **Audio forwarding** — PulseAudio capture → Opus → browser playback
- **GPU encoding** — NVENC/VAAPI for 4K60 with minimal CPU
- **Hardware decode** — DXVA2/VideoToolbox/VA-API on native client

## License

MIT
