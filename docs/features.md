# Phantom — Implemented Features

55 features across capture, encode, transport, audio, input, security, and
deployment. New features in 0.4.0 are flagged at the bottom.

## Encode + Capture

| # | Feature |
|---|---------|
| 1 | H.264 encoding (OpenH264 CPU, `--encoder` plugin architecture) |
| 2 | H.264 full frames + lossless refinement after 2s idle (tile path disabled — caused tearing) |
| 3 | Periodic keyframe (2s interval, recovers from loss / decoder errors) |
| 19 | Encoder plugin architecture (`--encoder` flag, `Box<dyn FrameEncoder>`) |
| 20 | NVENC GPU encoding (`--encoder nvenc`, runtime dlopen, no build-time CUDA dep) |
| 21 | NVFBC GPU capture (`--capture nvfbc`, zero-copy CUdeviceptr) |
| 22 | NVFBC→NVENC zero-copy pipeline (capture+encode ~4ms at 1080p on A40) |
| 23 | Windows support (DXGI capture, OpenH264/NVENC, enigo input) |
| 27 | DXGI→NVENC zero-copy (`--capture dxgi --encoder nvenc`, D3D11 texture, no CPU copy) |
| 35 | AV1 encoder (NVENC hardware AV1, Ada Lovelace+, `--codec av1`) |
| 36 | AV1 decoder (dav1d software + NVDEC hardware, WebCodecs for web) |
| 37 | NVDEC hardware decode (client-side H.264 + AV1, runtime dlopen, feature-gated) |
| 28 | VideoToolbox hardware decode (macOS native client, 2-2.5x faster at 4K) |
| 29 | 4K support (bilinear downscale, aspect-ratio letterbox, coordinate mapping) |
| 33 | AVX2 SIMD color conversion (BGRA→NV12 2.8x, YUV→RGB32 3.4x faster, runtime-detected) |

## Transport + Networking

| # | Feature |
|---|---------|
| 4 | ChaCha20-Poly1305 encryption (TCP) / TLS (QUIC) / DTLS (WebRTC) |
| 5 | QUIC/UDP transport (quinn) |
| 6 | TCP transport with optional encryption |
| 14 | Web client WebRTC (DataChannel + chunking, WebCodecs, Canvas, POST /rtc) |
| 15 | Web client WSS fallback (`?ws` URL param, same HTTPS port) |
| 25 | Self-signed HTTPS (rcgen, enables WebCodecs on non-localhost) |
| 31 | QUIC ALPN fix (client was missing ALPN protocol — QUIC never worked before) |
| 32 | HTTP keep-alive + connection pool (reuses TLS connections, bounded 16-thread pool) |
| 38 | Adaptive bitrate (baseline RTT tracking, congestion-based decrease, NVENC reconfigure API) |
| 39 | Connection quality stats (Stats message every 5s: RTT, FPS, bandwidth, encode_us) |
| 41 | RTT measurement (Ping/Pong, server EMA α=0.2) |
| 43 | Forward-compatible protocol (`read_message_lenient`, skips unknown message variants) |

## Session lifecycle

| # | Feature |
|---|---------|
| 9 | Auto-reconnect (exponential backoff) |
| 12 | Adaptive quality (congestion-based frame skipping) |
| 38 | Adaptive bitrate (see above) |
| **0.4.0** | ClientHello session affinity + ghost-set rejection (Linux + Windows service mode) |
| **0.4.0** | Pre-flight resolution hint (Windows service mode — no open-flicker) |
| **0.4.0** | Session correlation IDs (8-char hex per session, in every stats line) |
| **0.4.0** | Structured `SessionEndReason` enum (Cancelled / PeerClosed / NetworkError / etc.) |

## Input + I/O

| # | Feature |
|---|---------|
| 7 | Clipboard sync (bidirectional, arboard) |
| 8 | Ctrl+V paste (client intercepts → server `enigo.text()`) |
| 11 | Window scaling + coordinate mapping |
| 16 | Hidden remote cursor (mouse move = 0 CPU) |
| 46 | Scroll redesign (Sunshine-style pixel accumulation, client-native direction) |
| 53 | Clipboard paste in service mode (`PasteText` → IPC → agent `enigo.text()`) |
| 54 | Clipboard sync in service mode (agent arboard polling → IPC → `ClipboardSync`) |
| 55 | File transfer toast + path (`FileSaved` protocol message, batch progress) |

## Audio

| # | Feature |
|---|---------|
| 42 | Web audio (Opus decode via WebCodecs `AudioDecoder`, auto-resume on gesture) |
| (impl.) | Server: PulseAudio monitor (Linux) + WASAPI loopback (Windows) → Opus 48kHz stereo |
| (impl.) | Client: cpal ring buffer with prime threshold + soft drain + underrun/trim metrics |
| **0.4.0** | Audio drop counter on the capture side (visible in stats) |

## Security

| # | Feature |
|---|---------|
| 45 | JWT token auth (`--auth-secret`, HMAC-SHA256, platform signs `{sub, vm_id, exp}`, HTTP 401 on invalid) |

## Native client

| # | Feature |
|---|---------|
| 13 | Native client (winit + softbuffer, OS key repeat) |
| 48 | Native UI: borderless fullscreen, macOS transparent title bar, F11/Esc toggle |
| **0.4.0** | macOS top-edge gradient backdrop for traffic lights |
| **0.4.0** | Native client sends preferred resolution on connect (no post-open resize round-trip) |

## Deployment

| # | Feature |
|---|---------|
| 17 | Docker XFCE test environment |
| 18 | Mock server (test without screen capture) |
| 24 | Auto-start (Windows: schtasks ONLOGON, Linux: systemd) |
| 26 | WASM pkg in repo (Windows builds without wasm-pack) |
| 30 | Cross-platform release pipeline (GitHub Actions: Linux/Windows/macOS/Docker, install.sh, install.ps1) |
| 44 | Windows Service mode (Session 0 service + agent, IPC pipe, lock screen GDI fallback) |
| 49 | Virtual Display Driver (VDD) auto-install (`--install` downloads MiketheTech VDD + nefcon) |
| 50 | TCC→WDDM auto-switch (`--install` detects NVIDIA GPU mode, switches if needed) |
| 51 | Adaptive resolution (web client viewport → agent `ChangeDisplaySettingsEx`, 1.3x scale, 300ms debounce) |
| 52 | DXGI VDD device targeting (capture from VDD by device name like DCV/Parsec) |

## Observability

| # | Feature |
|---|---------|
| 40 | Server stats logging (GPU stats with FPS, RTT, bandwidth, encode time) |
| **0.4.0** | `--log-file` + `--log-rotate` + `--log-keep` (rotating file appender) |
| **0.4.0** | Network jitter EMA in stats line |
| **0.4.0** | Audio drops counted per stats interval |
| **0.4.0** | 16 new unit tests (doorbell, classify_session_error, ClientHello round-trip) |

## Testing

| # | Feature |
|---|---------|
| 34 | WAN simulation tests (8 E2E tests: 0–300ms RTT, jitter, encrypted, keepalive, session replacement) |
| (impl.) | 134 tests across all crates (workspace `cargo test`) |
