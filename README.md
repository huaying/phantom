# Phantom Remote Desktop

A high-performance, open-source remote desktop built in Rust. Pixel-perfect text, low latency, browser and native access, single binary deployment.

## Features

- **H.264 streaming** with three-phase rendering (lossy H.264 → lossless tiles → pixel-perfect refinement)
- **Smart encoding** — small changes use lightweight tiles, large changes use H.264 (90% CPU savings)
- **Web client via WebRTC DataChannel** — connect from any browser, zero install, UDP-based (no jitter buffer)
- **Native client** — winit + softbuffer with local cursor rendering
- **QUIC/UDP transport** — no head-of-line blocking on WAN
- **Encrypted by default** — ChaCha20-Poly1305 (TCP) or TLS (QUIC) or DTLS (WebRTC)
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

### Web Client (WebRTC)

```bash
# Server with web access
cargo run --release -p phantom-server -- --transport web --no-encrypt

# Open in browser (Chrome/Edge/Safari)
# → http://<server-ip>:9900
```

### Docker (test environment with XFCE desktop)

```bash
docker build -t phantom .

# Native client mode
docker run --rm -p 9900:9900 phantom server
cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900

# Web client mode (WebRTC)
docker run --rm -p 9900:9900 -p 9901:9901 -p 9902:9902/udp \
  -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open http://127.0.0.1:9900
```

## Architecture

```
Native Client                        Server                         Web Client (Browser)
┌──────────────┐   QUIC/TCP   ┌──────────────────┐  WebRTC DC   ┌──────────────┐
│ OpenH264     │◄════════════╗│ Screen Capture    │╗═══(UDP)═══►│ WASM (207KB) │
│ winit render │             ║│ Smart Encode:     │║             │ WebCodecs    │
│ Local cursor │             ║│  <10% → zstd tiles│║             │ Canvas       │
│ OS key repeat│             ║│  ≥10% → H.264     │║             │ Input capture│
│              │═════════════╝│  static → lossless │╝═════════════│              │
│ Input capture│──────────────│ enigo inject      │──────────────│ Keyboard/    │
│              │              │                    │  POST /rtc   │ Mouse events │
└──────────────┘              └──────────────────┘  (signaling)  └──────────────┘
```

### Web Client Transport: WebRTC DataChannel

The web client uses WebRTC DataChannel (not WebSocket) for data transport:

- **No jitter buffer** — unlike WebRTC Media Tracks, DataChannels deliver bytes directly
- **Signaling via HTTP POST** — browser POSTs SDP offer to `/rtc`, server returns answer
- **3 DataChannels**: video (H.264 frames + tiles), input (mouse/keyboard), control (Hello/clipboard)
- **str0m** (sans-IO WebRTC) on the server, native `RTCPeerConnection` in browser
- **WebSocket fallback** preserved for future adaptive mode

## Server Options

```
--listen <addr>           Listen address (default: 0.0.0.0:9900)
--transport <tcp|quic|web>  Transport protocol (default: tcp)
--fps <n>                 Target FPS (default: 30)
--bitrate <kbps>          H.264 bitrate (default: 5000)
--quality-delay-ms <ms>   Lossless update delay (default: 2000)
--encoder <name>          Video encoder (default: openh264)
--key <hex>               Encryption key (auto-generated if omitted)
--no-encrypt              Disable encryption
```

Environment variables (for Docker):
- `PHANTOM_HOST` — IP address for WebRTC ICE candidate (default: auto-detect)

## How It Works

### Three-Phase Rendering
1. **Small change** (<10% dirty) → zstd compressed tiles only (0.1ms CPU)
2. **Large change** (≥10% dirty) → H.264 full frame (15ms CPU)
3. **Static** (2s no change) → zstd lossless full update (pixel-perfect)

### Smart Encoding
Server detects dirty regions and chooses the cheapest encoding:
- Typing/cursor → tiles (0.1ms)
- Scrolling/video → H.264 (15ms)
- Mouse cursor hidden server-side → mouse movement = 0 CPU

Result: 2-core cloud VM uses ~3% CPU for typical office work.

### WebRTC DataChannel vs WebSocket
| | WebSocket | WebRTC DataChannel |
|--|-----------|-------------------|
| Transport | TCP | UDP (SCTP over DTLS) |
| Latency | 50-150ms (HOL blocking) | 20-50ms |
| Jitter buffer | N/A | None (raw delivery) |
| Encryption | Optional (ChaCha20) | Built-in (DTLS) |
| Signaling | N/A | Single HTTP POST |

## Project Structure

```
phantom/
├── crates/
│   ├── core/      Traits, protocol, tile differ, color, crypto, clipboard
│   ├── server/    Capture, encode, input inject, TCP/QUIC/WS/WebRTC transports
│   ├── client/    Decode, winit display, input capture, reconnect
│   └── web/       WASM client (WebCodecs, Canvas, WebRTC DataChannel)
├── Dockerfile     XFCE desktop test environment
├── DESIGN.md      Full design document + roadmap
└── README.md
```

### Trait-Based Extensibility

All components are swappable via traits:

| Trait | Current | Future |
|-------|---------|--------|
| `FrameCapture` | scrap (CPU) | NVFBC, DMA-BUF |
| `FrameEncoder` | OpenH264 (CPU) | NVENC, VAAPI, x264 |
| `MessageSender/Receiver` | TCP, QUIC, WS, WebRTC | — |

Adding a new backend = implement the trait + one-line init change. Session loop untouched.

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

- **Audio forwarding** — PulseAudio capture → Opus → browser playback
- **GPU encoding** — NVENC/VAAPI for 4K60 with minimal CPU
- **WS/WebRTC adaptive fallback** — auto-detect best transport
- **Wayland capture** — PipeWire for modern Linux

## License

MIT
