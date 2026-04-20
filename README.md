# Phantom Remote Desktop

A high-performance, open-source remote desktop in Rust. Browser and native access, single-binary deployment.

## Features

- **H.264 / AV1 streaming** — OpenH264 CPU + NVENC GPU (NVIDIA); zero-copy GPU paths (NVFBC→NVENC on Linux, DXGI→NVENC on Windows)
- **Browser client** — zero install, WebCodecs decode via WebSocket over HTTPS
- **Native client** — winit + softbuffer, with NVDEC / dav1d / VideoToolbox hardware decode where available
- **Adaptive bitrate + congestion control** — RTT-based, per-session
- **Audio** — PulseAudio (Linux) / WASAPI (Windows) → Opus → client playback
- **Input forwarding** — `/dev/uinput` on Linux (survives GDM 42 scramble + works under Wayland); enigo fallback
- **Clipboard + file transfer** — bidirectional, SHA-256 verified file push
- **Encrypted by default** — ChaCha20-Poly1305 for TCP; TLS for QUIC + WebSocket; DTLS for WebRTC
- **Session resume + replacement** — opaque tokens, seamless reconnect, new-client takeover
- **Production logging** — `--log-file` + daily rotation, structured session_id / jitter / audio_drops stats
- **Windows Service mode** — pre-login access via SCM Session 0 service + user-session agent IPC; Virtual Display Driver auto-install for headless GPU servers
- **Linux VM auto-setup** — `install.sh --autologin` wires GDM autologin + watchdog so phantom survives sign-out

## Install

### One-line install

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
```

**Windows** (elevated PowerShell):
```powershell
irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex
```

For a dedicated Linux remote-access VM (autologin + no screen lock + watchdog):
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh -s server --autologin
```

### Pre-built binaries

From [GitHub Releases](https://github.com/huaying/phantom/releases):

| Platform | Server | Client |
|----------|--------|--------|
| Linux x86_64 | `phantom-server-linux-x86_64` | `phantom-client-linux-x86_64` |
| Windows x86_64 | `phantom-server-windows-x86_64.exe` | `phantom-client-windows-x86_64.exe` |
| macOS x86_64 / ARM | — | `phantom-client-macos-{x86_64,aarch64}` |

### Auto-start on boot

**Windows** (Windows Service + Virtual Display Driver, run in elevated PowerShell):
```powershell
phantom-server.exe --install        # register service + install VDD
phantom-server.exe --install-vdd    # re-run just the VDD step if it failed
phantom-server.exe --uninstall      # remove everything
```

**Linux** (systemd user unit):
```bash
phantom-server --install
phantom-server --uninstall
```

### Docker

```bash
docker build -t phantom .
docker run --rm -p 9900:9900 -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open https://127.0.0.1:9900
```

## Usage

```bash
# Web server (default, https://<server-ip>:9900 in Chrome/Edge)
phantom-server --transport web --no-encrypt

# GPU accelerated (NVIDIA)
phantom-server --transport web --no-encrypt --capture nvfbc --encoder nvenc  # Linux
phantom-server.exe --transport web --no-encrypt --capture dxgi --encoder nvenc --fps 60  # Windows

# Native client
phantom-server                               # server prints --key <hex>
phantom-client -c <server-ip>:9900 --key <hex>
```

See [`docs/features.md`](docs/features.md) for the full CLI reference and [`docs/architecture.md`](docs/architecture.md) for design rationale.

## Build from source

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Linux build deps
sudo apt-get install -y libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
    libxdo-dev nasm libpulse-dev libopus-dev

# WASM web client first (server embeds the bundle via include_bytes!)
wasm-pack build crates/web --target web --no-typescript

cargo build --release
cargo build --release --features webrtc   # +WebRTC DataChannel
cargo test --workspace                     # 136 tests
cargo clippy --workspace -- -D warnings
```

## License

MIT
