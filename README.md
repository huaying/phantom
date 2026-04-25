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

One line, and you're done. The install scripts download the binary **and** wire
up auto-start — no second command, no "now run `--install`" step.

**Linux** (one box, your current user):
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
```
Installs `phantom-server` to `/usr/local/bin`, pulls runtime libraries, sets
up `/dev/uinput` for keyboard injection, and drops an XDG autostart entry so
`phantom-server` starts at your next graphical login. To start it immediately
in the current session, just run `phantom-server`.

**Linux** (dedicated remote-access VM — survives sign-out, no screen lock):
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh -s server --autologin --user=$USER
```
Layers GDM autologin + screen-lock disable + a systemd watchdog on top of
the default autostart. After the next reboot the VM comes up logged in with
phantom already serving, and will recover itself on sign-out. The installer
runs a post-install doctor by default; re-run it after reboot with:

```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh -s -- --doctor --doctor-strict --user=$USER
```

The doctor checks OS display state and runs `phantom-server --probe-capture`,
which initializes the resolved capture/encoder path, captures one frame, rejects
mostly-black output, and verifies the frame can be encoded.

For first-boot VM provisioning, use [`docs/cloud-init/phantom-server.yaml`](docs/cloud-init/phantom-server.yaml)
as the starting point.

**macOS** (client only):
```bash
curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
```

**Windows** (elevated PowerShell — required to register the service):
```powershell
irm https://raw.githubusercontent.com/huaying/phantom/main/install.ps1 | iex
```
Downloads the binaries to `%LOCALAPPDATA%\phantom`, adds them to PATH,
registers the Phantom Windows Service (LocalSystem / auto-start) and
installs the Virtual Display Driver. If you run this from a **non-elevated**
shell the download still succeeds but service registration is skipped and
the script tells you how to finish with `phantom-server.exe --install`.

### Opt-outs and knobs

| You want | Command |
|---|---|
| Install but don't touch autostart (Linux) | `... \| sh -s server --no-autostart` |
| Install for a specific Linux login user | `... \| sh -s server --autologin --user=dev-user` |
| Run installer health checks only | `... \| sh -s -- --doctor --doctor-strict --user=dev-user` |
| Skip post-install health checks | `... \| sh -s server --no-doctor` |
| Install but don't register service (Windows) | `$env:PHANTOM_NO_AUTOSTART=1; irm ... \| iex` |
| Skip post-install health checks (Windows) | `$env:PHANTOM_NO_DOCTOR=1; irm ... \| iex` |
| Fail install command when Windows doctor fails | `$env:PHANTOM_DOCTOR_STRICT=1; irm ... \| iex` |
| Client only on Linux | `... \| sh -s client` |
| Server **and** client on one box | `... \| sh -s both` |
| Remove autostart (Linux) | `rm ~/.config/autostart/phantom-server.desktop` |
| Remove Windows Service | `phantom-server.exe --uninstall` (elevated) |

### Pre-built binaries (manual install)

From [GitHub Releases](https://github.com/huaying/phantom/releases):

| Platform | Server | Client |
|----------|--------|--------|
| Linux x86_64 | `phantom-server-linux-x86_64` | `phantom-client-linux-x86_64` |
| Windows x86_64 | `phantom-server-windows-x86_64.exe` | `phantom-client-windows-x86_64.exe` |
| macOS x86_64 / ARM | — | `phantom-client-macos-{x86_64,aarch64}` |

After copying the binary somewhere on PATH you can run
`phantom-server --install` (elevated on Windows) to get the same auto-start
the install scripts configure.

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
cargo build --release --features webrtc   # +WebRTC (media tracks + data channels)
cargo test --workspace                     # 136 tests
cargo clippy --workspace -- -D warnings
```

## License

MIT
