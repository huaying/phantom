# Phantom Remote Desktop

A high-performance, open-source remote desktop built in Rust. Low latency, browser and native access, single binary deployment.

## Features

- **H.264 streaming** with periodic keyframes and lossless refinement after 2s idle
- **GPU acceleration** — NVENC encoding + DXGI zero-copy capture (Windows), NVFBC→NVENC (Linux)
- **Audio forwarding** — PulseAudio → Opus 48kHz stereo → client playback
- **File transfer** — bidirectional, chunked streaming with SHA-256 integrity verification
- **Multi-monitor** — `--display N` to select monitor, `--list-displays` to enumerate
- **Hardware auto-detect** — `--encoder auto` probes GPU and picks best encoder/capture
- **Multi-transport** — `--transport tcp,web` serves TCP and HTTPS/WebSocket simultaneously
- **Web client via WebSocket** — connect from any browser, zero install, WebCodecs H.264 decode + Opus audio
- **Native client** — winit + softbuffer with local cursor rendering
- **QUIC/UDP transport** — for native client, no head-of-line blocking
- **Encrypted by default** — ChaCha20-Poly1305 (TCP) or TLS (QUIC) or DTLS (WebRTC)
- **Clipboard sync** — bidirectional, with Ctrl+V paste injection
- **Session replacement** — new client seamlessly takes over active session
- **Graceful shutdown** — Ctrl+C / SIGTERM clean exit
- **Auto-reconnect** — exponential backoff (native client)
- **HTTP keep-alive + connection pool** — reuses TLS connections, bounded thread pool (16 max)
- **SIMD color conversion** — AVX2-accelerated BGRA↔YUV (2.8–3.4x faster than scalar)
- **Connection quality stats** — RTT, FPS, bandwidth, encode time reported to client every 5s
- **Wayland capture** — PipeWire + XDG Desktop Portal (compile-tested)
- **WAN tested** — verified under simulated latency (0–300ms RTT), jitter, and session replacement
- **Windows + Linux** — DXGI (Windows) / X11 (Linux) capture, auto-start support

## Installation

### One-line install

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex
```

### Download pre-built binaries

Grab the latest binaries from [GitHub Releases](https://github.com/huaying/phantom/releases):

| Platform | Server | Client |
|----------|--------|--------|
| Linux x86_64 | `phantom-server-linux-x86_64` | `phantom-client-linux-x86_64` |
| Windows x86_64 | `phantom-server-windows-x86_64.exe` | `phantom-client-windows-x86_64.exe` |
| macOS ARM | — | `phantom-client-macos-aarch64` |
| macOS x86_64 | — | `phantom-client-macos-x86_64` |

```bash
# Example: download and run on Linux
chmod +x phantom-server-linux-x86_64
./phantom-server-linux-x86_64 --transport web --no-encrypt
```

### Docker

```bash
docker run --rm -p 9900:9900 ghcr.io/huaying/phantom:latest
# → open https://127.0.0.1:9900
```

Or build locally:

```bash
docker build -t phantom .
docker run --rm -p 9900:9900 -e PHANTOM_HOST=127.0.0.1 phantom server-web
```

### Build from source

See [Building](#building) below.

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
--transport <transports>     Comma-separated: tcp, web, quic (default: tcp,web)
--fps <n>                    Target FPS (default: 30)
--bitrate <kbps>             H.264 bitrate (default: 5000)
--quality-delay-ms <ms>      Lossless update delay (default: 2000)
--encoder <auto|openh264|nvenc>  Video encoder (default: auto)
--capture <auto|scrap|nvfbc|pipewire>  Screen capture (default: auto)
--display <n>                Display index to capture (default: 0)
--list-displays              List available displays and exit
--send-file <path>           Send a file to the first connected client
--key <hex>                  Encryption key (auto-generated if omitted)
--no-encrypt                 Disable encryption
--install / --uninstall      Auto-start (Windows: schtasks, Linux: systemd)
```

### Client Options

```
--connect <addr>             Server address (default: 127.0.0.1:9900)
--transport <tcp|quic>       Transport protocol (default: tcp)
--decoder <auto|openh264|videotoolbox>  Video decoder (default: auto)
--send-file <path>           Send a file to the server after connecting
--key <hex>                  Encryption key (from server output)
--no-encrypt                 Disable encryption
```

## Performance

| Configuration | FPS (1080p) | Notes |
|--------------|-------------|-------|
| OpenH264 CPU (scrap) | 6-8 | Any machine, fallback |
| NVENC GPU (scrap) | 17-18 | NVIDIA GPU, CPU capture |
| DXGI→NVENC zero-copy | 30-47 | Windows + NVIDIA, all GPU |
| NVFBC→NVENC zero-copy | ~60+ | Linux + NVIDIA (X11) |

### SIMD Color Conversion (AVX2)

| Operation | Scalar (1080p) | AVX2 SIMD | Speedup |
|-----------|---------------|-----------|---------|
| BGRA→NV12 (encode) | 5.1ms | 1.8ms | **2.8x** |
| YUV→RGB32 (decode) | 8.5ms | 2.5ms | **3.4x** |

Runtime-detected: AVX2 on x86_64, automatic scalar fallback on other architectures.

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

# Run tests (95 tests: unit, integration, E2E, WAN simulation)
cargo test
```

## Project Structure

```
phantom/
├── crates/
│   ├── core/      Traits, protocol, frame, input, clipboard, file transfer, SIMD color, crypto
│   ├── server/    Capture, encode, input inject, file transfer, TCP/QUIC/WSS transports
│   ├── client/    Decode, winit display, input capture, file transfer, reconnect
│   ├── web/       WASM client (WebCodecs, Canvas, WebSocket/WebRTC)
│   ├── gpu/       NVENC, NVFBC (Linux), DXGI capture (Windows), CUDA, hardware probe
│   └── bench/     Encoder benchmark (OpenH264 vs NVENC)
├── Dockerfile     XFCE desktop test environment
├── CLAUDE.md      Developer guide
└── DESIGN.md      Design document
```

## Roadmap

See [CLAUDE.md](CLAUDE.md) for the full roadmap. Key next steps:

- ~~Audio forwarding~~ ✅ PulseAudio → Opus → client playback
- ~~Hardware probe~~ ✅ Auto-detect best encoder/capture at startup
- ~~SIMD color conversion~~ ✅ AVX2-accelerated BGRA↔YUV (2.8–3.4x speedup)
- ~~Multi-monitor~~ ✅ `--display N` and `--list-displays`
- ~~File transfer~~ ✅ Bidirectional, chunked, SHA-256 verified
- ~~Session replacement~~ ✅ Seamless client takeover
- ~~Graceful shutdown~~ ✅ Clean Ctrl+C / SIGTERM handling
- ~~Wayland capture~~ ✅ PipeWire + XDG Desktop Portal (compile-tested)
- **Hardware decode** — NVDEC (Linux), VideoToolbox (macOS) client-side
- **AV1 encoder** — better compression ratio at same bitrate
- **QUIC unreliable datagrams** — lower latency for WAN
- **VAAPI/AMF GPU encoding** — AMD/Intel GPU encode support
- **NAT traversal** — STUN/TURN for firewall bypass

## License

MIT
