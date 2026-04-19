# Phantom Remote Desktop

A high-performance, open-source remote desktop built in Rust. Low latency, browser and native access, single binary deployment.

## Features

- **H.264 / AV1 streaming** — OpenH264 CPU + NVENC GPU (NVIDIA), periodic keyframes, dirty-tile gating
- **Zero-copy GPU pipelines** — DXGI→NVENC on Windows, NVFBC→NVENC on Linux
- **Web client** — zero-install browser access via WSS + WebCodecs; native client via TCP / QUIC
- **Audio forwarding** — PulseAudio / WASAPI → Opus 48kHz stereo
- **Adaptive bitrate** — RTT-based with hysteresis
- **Encrypted by default** — ChaCha20-Poly1305 (TCP) / TLS (QUIC) / DTLS (WebRTC)
- **Clipboard sync + file transfer** — bidirectional, Ctrl+V paste injection, SHA-256 verified
- **Session replacement + auto-reconnect** — new client takes over seamlessly, native client reconnects with backoff
- **Multi-monitor, multi-transport** — `--display N`, `--transport tcp,web,quic`

See [docs/features.md](docs/features.md) for the full capability matrix and
[docs/architecture.md](docs/architecture.md) for design rationale.

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

From [GitHub Releases](https://github.com/huaying/phantom/releases):

| Platform | Server | Client |
|----------|--------|--------|
| Linux x86_64 | `phantom-server-linux-x86_64` | `phantom-client-linux-x86_64` |
| Windows x86_64 | `phantom-server-windows-x86_64.exe` | `phantom-client-windows-x86_64.exe` |
| macOS ARM / x86_64 | — | `phantom-client-macos-{aarch64,x86_64}` |

### Auto-start on boot

**Windows** (Windows Service + MTT Virtual Display Driver; run in elevated PowerShell):
```powershell
phantom-server.exe --install      # register service + install VDD
phantom-server.exe --install-vdd  # re-run just the VDD step if it failed
phantom-server.exe --uninstall    # remove everything
```

**Linux** (systemd user unit):
```bash
phantom-server --install
phantom-server --uninstall
```

For Linux VMs where you want phantom to survive sign-out (auto-relogin via
GDM, no keyring popup, watchdog that kicks GDM if stuck at greeter):
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh -s server --autologin
```

### Docker

```bash
docker build -t phantom .
docker run --rm -p 9900:9900 -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open https://127.0.0.1:9900
```

## Quick Start

```bash
# Web access (default, https://<server-ip>:9900 in Chrome/Edge)
phantom-server --transport web --no-encrypt

# GPU acceleration (NVIDIA)
phantom-server --transport web --no-encrypt --capture nvfbc --encoder nvenc   # Linux
phantom-server.exe --transport web --no-encrypt --capture dxgi --encoder nvenc --fps 60   # Windows

# Native client
phantom-server                                # prints --key <hex>
phantom-client -c <server-ip>:9900 --key <hex>
```

## Architecture

```
 Native Client            Server                   Web Client (Browser)
┌──────────────┐   QUIC  ┌─────────────────┐  WSS  ┌──────────────┐
│ OpenH264/    │◄───────►│ Screen Capture  │◄─────►│ WASM client  │
│ dav1d/NVDEC  │   TCP   │ H.264/AV1       │       │ WebCodecs    │
│ winit render │         │ (OpenH264/NVENC)│       │ Canvas       │
│ cpal audio   │         │ Adaptive Bitrate│       │ Opus audio   │
│              │         │ Audio Capture   │       │              │
│ Input        │────────►│ enigo/uinput    │◄──────│ Input        │
└──────────────┘         └─────────────────┘       └──────────────┘
```

## CLI

### Server

```
--listen <addr>                        Listen address (default: 0.0.0.0:9900)
--transport <tcp,web,quic>             Comma-separated transports (default: tcp,web)
--fps <n>                              Target FPS (default: 30)
--bitrate <kbps>                       Initial bitrate (default: 5000)
--encoder <auto|openh264|nvenc>        Video encoder (default: auto)
--codec <auto|h264|av1>                Video codec (default: auto → H.264; AV1 opt-in)
--capture <auto|scrap|nvfbc|pipewire|dxgi>
                                       Screen capture (default: auto)
--display <n>                          Display index (0 = primary)
--list-displays                        Enumerate and exit
--send-file <path>                     Push a file to the first client
--key <hex> / --no-encrypt             Encryption key / disable
--stun <server|auto>                   NAT discovery (prints a connection code)
--public-addr <ip:port>                Override public address (skip STUN)
--install / --uninstall                Register / remove auto-start
--install-vdd                          (Windows) re-run just the VDD install step
--auth-secret <hex>                    HMAC-SHA256 secret for JWT auth (WebSocket)
--log-file <path> / --log-rotate / --log-keep
                                       File logging with rotation
```

### Client

```
--connect <addr>                       Server address
--transport <tcp|quic>                 Transport (default: tcp)
--decoder <auto|openh264|dav1d|nvdec|videotoolbox>
                                       Video decoder (default: auto)
--send-file <path>                     Push a file to the server on connect
--key <hex> / --no-encrypt             Encryption key / disable
```

## Performance

Rough numbers at 1080p; see `cargo run --release -p phantom-bench` on your hardware.

| Configuration | FPS | Notes |
|---|---|---|
| OpenH264 CPU (scrap) | 6–8 | Any machine, fallback |
| NVENC GPU (scrap) | 17–18 | NVIDIA + CPU capture |
| DXGI→NVENC zero-copy | 30–47 | Windows + NVIDIA |
| NVFBC→NVENC zero-copy | ~60+ | Linux + NVIDIA (X11) |

### SIMD color conversion (AVX2 on x86_64, scalar fallback elsewhere)

| Operation | Scalar (1080p) | AVX2 | Speedup |
|---|---|---|---|
| BGRA→NV12 | 5.1ms | 1.8ms | 2.8× |
| YUV→RGB32 | 8.5ms | 2.5ms | 3.4× |
| NV12→RGB32 | ~15ms | ~4ms | ~3.5× |

## Building

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Linux build deps
sudo apt-get install -y libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
    libxdo-dev nasm libpulse-dev libopus-dev

# WASM web client first (server embeds the bundle via include_bytes!)
wasm-pack build crates/web --target web --no-typescript

# Workspace
cargo build --release
cargo build --release --features webrtc   # +WebRTC DataChannel
cargo test                                 # 129 tests
cargo clippy --workspace -- -D warnings
```

## Project layout

```
phantom/
├── crates/
│   ├── core/     Traits, protocol, frame, input, clipboard, file transfer, SIMD color, crypto
│   ├── server/   Capture, encode, input inject, file transfer, TCP/QUIC/WSS transports
│   ├── client/   Decode, winit display, input capture, reconnect
│   ├── web/      WASM client (WebCodecs, Canvas, WebSocket/WebRTC)
│   ├── gpu/      NVENC / NVDEC / NVFBC / DXGI capture (feature-gated)
│   └── bench/    Encoder benchmark
├── docs/         Architecture, features, file-map, pitfalls
├── CLAUDE.md     AI assistant guide
└── DESIGN.md     Design pointer
```

## License

MIT
