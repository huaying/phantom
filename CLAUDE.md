# Phantom — Developer Guide for Claude

## What This Project Is

Phantom is a high-performance, open-source remote desktop built in Rust. Target: Parsec-class latency (~20-50ms) with DCV-class quality (pixel-perfect text), single binary deployment, browser + native access.

~6,000 lines Rust, 21 tests, MIT license.

## Build Commands

```bash
cargo build --release                                    # native
wasm-pack build crates/web --target web --no-typescript  # WASM (must run BEFORE server build!)
cargo build --release -p phantom-server                  # server embeds WASM via include_bytes!
cargo test                                               # 21 tests
cargo clippy --release                                   # must be zero warnings

# GPU benchmarks (requires NVIDIA GPU + DISPLAY=:0)
DISPLAY=:0 cargo run --release -p phantom-bench          # encoder comparison: openh264 vs nvenc
DISPLAY=:0 cargo run --release --example nvenc_bench -p phantom-gpu  # GPU unit tests
```

**IMPORTANT**: WASM build order matters. Server embeds `crates/web/pkg/phantom_web_bg.wasm` via `include_bytes!`. If you change WASM code, rebuild WASM first, then server.

## Test Environment

```bash
# Docker (CPU-only):
docker build -t phantom .
docker run --rm -p 9900:9900 -p 9901:9901 -p 9902:9902/udp -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open http://127.0.0.1:9900

# Native client:
docker run --rm -p 9900:9900 phantom server
cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900

# GPU test VM (A40, driver 550, Ubuntu 24.04):
ssh horde@10.57.233.13
DISPLAY=:0 cargo run --release -p phantom-bench           # full encoder benchmark
DISPLAY=:0 cargo run --release --example nvenc_bench -p phantom-gpu  # GPU unit tests
# Server with GPU:
DISPLAY=:0 phantom-server --encoder nvenc --capture nvfbc --transport web
```

---

## Competitive Position

```
                    Latency     Text Quality    Deploy        Web Client  Open Source
Parsec              15-30ms     lossy(blurry)   simple        ❌          ❌
NICE DCV            30-60ms     pixel-perfect   medium        ✅(limited) ❌
KasmVNC             80-150ms    pixel-perfect   Docker        ✅          ✅
Neko                80-150ms    lossy(blurry)   Docker        ✅          ✅
Selkies (Google)    70-120ms    lossy(blurry)   complex       ✅          ✅
RustDesk            50-100ms    lossy(blurry)   simple        ✅(beta)    ✅
Phantom (target)    20-50ms     pixel-perfect   single binary ✅          ✅
```

### Our 5 Unique Advantages
1. **Three-phase rendering** — no open-source competitor has pixel-perfect text + low latency
2. **WebRTC DataChannel + WebCodecs** — no jitter buffer, 2x faster than Neko/Selkies in browser
3. **Single binary** — web client embedded, no Docker/GStreamer/coturn needed
4. **Rust WASM code sharing** — one codebase for server + web client
5. **~4,600 lines** — vs KasmVNC 200K+, Neko 15K+, RustDesk 150K

### Key Lessons From Competitors
- **DCV**: two-phase rendering (we have it), QUIC transport (we have it), GPU sharing (future)
- **KasmVNC**: per-rectangle quality tracking, multi-encoder mixing, FFmpeg dlopen, DLP features
- **Sunshine**: zero-copy GPU pipeline (NVFBC→CUDA→NVENC), AV1, frame pacing
- **Parsec**: client-side prediction, BUD protocol, 1000Hz input
- **RustDesk**: NAT traversal, P2P, file transfer

---

## Architecture Decisions (why we chose what we chose)

### No GStreamer
Direct function calls (capture → encode → send) = 0ms pipeline overhead. Sunshine and Parsec don't use GStreamer either. Our pipeline is 3 steps, not 20 elements with buffer copies.

### WebRTC DataChannel, not Media Track
Media Track adds 30-80ms jitter buffer (designed for video calls). DataChannel delivers raw bytes instantly → WebCodecs GPU decode → Canvas. Measured: 20-50ms vs 80-150ms.

### WebRTC, not WebTransport
WebTransport requires HTTPS + certificates. Self-signed ≤14 days in Chrome. Pure IP (most users) doesn't work. WebRTC works with any IP, has built-in DTLS + NAT traversal.

### HTTP POST signaling (str0m pattern)
Browser creates offer → POST /rtc → server returns answer. Single HTTP round-trip. No WebSocket signaling needed. Avoids chicken-and-egg (session must run before signaling can work).

### str0m (sans-IO WebRTC)
Pure Rust, ~15K lines, no tokio for WebRTC path. We provide UDP socket, str0m provides logic. Official `chat.rs` pattern: one socket, one run_loop, demux via `rtc.accepts()`.

### Smart encoding (dirty% threshold)
CPU-only hosts: 90% of updates are small (typing, cursor). Tiles with zstd = 0.1ms vs H.264 = 15ms. Server hides remote cursor → mouse movement = 0 dirty tiles = 0 CPU.

### Three-phase rendering
1. Small change (<10% dirty) → zstd tiles only (0.1ms)
2. Large change (≥10% dirty) → H.264 full frame (15ms)
3. Static 2s → zstd lossless all tiles (pixel-perfect)

### Dual-track network
- Native: raw QUIC (no browser overhead, 15-30ms target)
- Browser: WebRTC DataChannel (sandboxed but 20-50ms target)
- Both produce same `Box<dyn MessageSender/Receiver>` → same session loop

---

## Key Implementation Details

### WebRTC run_loop (str0m official pattern)
- **One UDP socket** for entire server lifetime (never rebind — this was a hard bug)
- **One `run_loop` thread** managing one active client at a time
- New POST /rtc → drain all pending Rtc, keep latest → replace active client immediately
- Session delivered via `Mutex<Option>` slot (always latest, stale auto-dropped)
- Bounded `sync_channel(30)` for video with `try_send` (backpressure, no blocking)

### Session reconnect (hard-won bugs)
These 4 bugs took significant debugging. Don't reintroduce them:

1. **recv_msg() infinite spin**: MUST detect `mpsc::TryRecvError::Disconnected` and return error. Otherwise receive_loop thread spins forever, session never ends, no reconnect.

2. **Hello ordering**: Hello MUST go through video DC (same as VideoFrame). Control DC may deliver slower → decoder not configured when keyframe arrives → "Key frame is required" error.

3. **UDP socket lifecycle**: Do NOT create one socket per session. One socket for the whole server. str0m run_loop manages it. Old approach (bind/rebind per session) caused port conflicts.

4. **force_keyframe at session start**: New client needs IDR frame. Call `video_encoder.force_keyframe()` + `differ.reset()` at the top of `run_session()`.

### Smart encoding flow
```
capture → TileDiffer (64x64 blocks) → dirty count
  if dirty < 10% → zstd tiles only (TileUpdate)
  if dirty ≥ 10% → H.264 full frame (VideoFrame)
  if static 2s   → zstd lossless all tiles (quality refinement)
```

### Transport abstraction
`run_session()` takes `Box<dyn MessageSender>` + `Box<dyn MessageReceiver>`. All transports (TCP, QUIC, WebSocket, WebRTC) implement same traits. Adding new transport = new file + implement traits + one-line init change.

### GPU pipeline (crates/gpu/)
All NVIDIA libraries loaded at runtime via dlopen — compiles on any machine, GPU optional.

**NVENC encoder flow** (CPU input path):
```
Frame.data (BGRA CPU) → bgra_to_nv12 (CPU) → cuMemcpyHtoD → NVENC encode (GPU) → H.264 bytes
~10ms at 1080p
```

**NVFBC→NVENC zero-copy flow** (all GPU):
```
NVFBC grab → CUdeviceptr (NV12, GPU) → NVENC encode (GPU) → H.264 bytes
~4ms at 1080p (capture 0.4ms + encode 3.5ms)
```

**CUDA context management** (hard-won lessons):
- Use `cuDevicePrimaryCtxRetain` — NVFBC internally uses the primary context. `cuCtxCreate` creates a separate context that conflicts.
- NVFBC holds a context lock. Must call `NvFBCReleaseContext` before NVENC operations, `NvFBCBindContext` before NVFBC grab.
- NVENC's `encode_registered()` checks `ctx_get_current()` and only does `ctx_push` if needed (avoids double-push deadlock).

**NVFBC struct sizes** (critical):
- NVFBC embeds sizeof in the version field. Wrong size = buffer overflow = silent memory corruption.
- Verified sizes from nvfbc-sys bindgen: CreateHandleParams=40, CaptureSessionParams=64, GrabFrameParams=32, FrameGrabInfo=48.
- Use opaque byte arrays (not Rust structs with named fields) to guarantee correct sizes.

**NVFBC function loading**:
- Do NOT use `NvFBCCreateInstance` — it has strict API version checks that vary by driver.
- Instead, dlsym each function directly: `NvFBCCreateHandle`, `NvFBCToCudaGrabFrame`, etc.

**Benchmark results** (A40 GPU, 1080p, driver 550):
```
OpenH264 (CPU):           47ms  (baseline)
NVENC (CPU color conv):   10ms  (4.7x faster)
NVFBC→NVENC (zero-copy):   4ms  (12x faster)
```

---

## Common Pitfalls

- **WASM build order**: must `wasm-pack build` BEFORE `cargo build -p phantom-server`
- **Docker WebRTC**: needs `-p 9902:9902/udp` AND `-e PHANTOM_HOST=127.0.0.1`
- **str0m Receive.destination**: must match candidate_addr (127.0.0.1:9902), not socket bind addr (0.0.0.0:9902)
- **macOS Cmd key**: don't send Meta/Super to server — gets stuck after Cmd+Tab
- **Mutex poison**: use `unwrap_or_else(|e| e.into_inner())` not `.unwrap()`
- **Bounded channels**: video uses `sync_channel(30)` + `try_send` — drops on full, never blocks
- **Keepalive**: 1s ping via `sender.send_msg(Ping)` detects dead channels after browser refresh
- **XFCE Super shortcuts**: removed in Docker entrypoint (conflict with macOS Cmd)
- **NVFBC struct sizes**: must match driver's expected sizeof exactly. Use opaque byte arrays, not Rust structs.
- **NVFBC FORCE_REFRESH**: blocks on driver 550. Use NOWAIT + ensure screen activity for new frames.
- **NVFBC needs DISPLAY**: set `DISPLAY=:0` when running on remote machine. NVFBC captures X11 framebuffer.
- **NVFBC + NVENC CUDA context**: use primary context (`cuDevicePrimaryCtxRetain`), not `cuCtxCreate`. Bind/release around NVFBC↔NVENC transitions.
- **NVENC GUID by value**: `nvEncGetEncodePresetConfigEx` passes GUIDs by value, not by pointer (C ABI).

---

## Implemented Features (22)

| # | Feature |
|---|---------|
| 1 | H.264 encoding (OpenH264 CPU, `--encoder` plugin architecture) |
| 2 | Three-phase rendering (tiles → H.264 → lossless) |
| 3 | Smart encoding (dirty% threshold, 90% CPU savings) |
| 4 | ChaCha20-Poly1305 encryption (TCP) / TLS (QUIC) / DTLS (WebRTC) |
| 5 | QUIC/UDP transport (quinn) |
| 6 | TCP transport with optional encryption |
| 7 | Clipboard sync (bidirectional, arboard) |
| 8 | Ctrl+V paste (client intercepts → server enigo.text()) |
| 9 | Auto-reconnect (exponential backoff) |
| 10 | Local cursor rendering (zero-latency) |
| 11 | Window scaling + coordinate mapping |
| 12 | Adaptive quality (congestion-based frame skipping) |
| 13 | Native client (winit + softbuffer, OS key repeat) |
| 14 | Web client WebRTC (DataChannel, WebCodecs, Canvas, POST /rtc signaling) |
| 15 | Web client WS fallback (preserved for adaptive mode) |
| 16 | Hidden remote cursor (mouse move = 0 CPU) |
| 17 | Docker XFCE test environment |
| 18 | Mock server (test without screen capture) |
| 19 | Encoder plugin architecture (--encoder flag, Box<dyn FrameEncoder>) |
| 20 | **NVENC GPU encoding** (`--encoder nvenc`, runtime dlopen, no build-time CUDA dep) |
| 21 | **NVFBC GPU capture** (`--capture nvfbc`, zero-copy CUdeviceptr) |
| 22 | **NVFBC→NVENC zero-copy pipeline** (capture+encode ~4ms at 1080p on A40) |

---

## Roadmap

### Immediate
| Task | Impact | Notes |
|------|--------|-------|
| ~~NVENC GPU encoding~~ | ✅ done | encode 47ms→10ms (CPU path), 4ms (zero-copy) |
| ~~NVFBC GPU capture~~ | ✅ done | zero-copy CUdeviceptr, ~0.4ms capture |
| **Integrate GPU pipeline into server** | full end-to-end | Wire `--capture nvfbc --encoder nvenc` into run_session zero-copy loop |
| **Hardware probe** | auto-detect GPU at startup | Select best encoder/capture automatically |
| **Audio forwarding** | meetings, media | PulseAudio capture → Opus encode → WebRTC/native |
| **WAN testing** | verify real latency | Need cloud VM, `tc netem` for simulating loss/delay |

### Host Performance
| Task | Impact |
|------|--------|
| VAAPI GPU encoding | AMD/Intel GPU encode |
| x264 via FFmpeg | 2-3x better compression than OpenH264 |
| AV1 encoding (NVENC/SVT-AV1) | 30% better than H.264 |
| DMA-BUF/KMS capture | Linux zero-copy |
| SIMD color conversion | 4x faster YUV↔RGB |

### Native Client Performance
| Task | Impact |
|------|--------|
| QUIC Unreliable Datagram | video over datagram, no retransmit |
| 0-RTT reconnect | instant reconnect on network switch |
| Hardware decode (DXVA2/VideoToolbox/VA-API) | decode 10ms→1ms |
| GPU direct render (wgpu) | zero-copy display |

### Features
| Task | Impact |
|------|--------|
| WS/WebRTC adaptive fallback | auto-detect best transport |
| Wayland capture (PipeWire) | modern Linux |
| Multi-monitor | dev setups |
| File transfer | drag-and-drop |
| NAT traversal (STUN/TURN) | firewall bypass |

### Enterprise
| Task | Impact |
|------|--------|
| GPU sharing (OpenGL interposition) | cloud workstations |
| DLP (watermark, clipboard control) | enterprise security |
| Session recording | audit/training |
| Protocol multiplexing | same port, auto-detect client type |

---

## Technical Debt

| Item | Severity |
|------|----------|
| BGRA→YUV via `pixel_f32()` (slow per-pixel callback) | Medium |
| Client threads leak on reconnect (no JoinHandle tracking) | Medium |
| No graceful shutdown (Ctrl+C) | Low |
| HTTP handler threads unbounded (no pool) | Medium |
| WS IO loop 5ms latency floor | Low |
| Mock server lacks encryption/input | Low |

---

## File Map

```
crates/core/src/
  lib.rs          Module exports
  capture.rs      FrameCapture trait
  encode.rs       FrameEncoder + Encoder (tile) + FrameDecoder traits
  decode.rs       Decoder trait (tile)
  transport.rs    MessageSender/Receiver/Connection traits
  display.rs      Display trait
  protocol.rs     Message enum, wire framing (bincode, length-prefixed)
  tile.rs         TileDiffer (64x64 dirty detection, sampling fast-path)
  frame.rs        Frame struct, PixelFormat
  input.rs        InputEvent, KeyCode, MouseButton
  clipboard.rs    ClipboardTracker (echo-loop prevention)
  color.rs        BGRA↔YUV420 conversion (BT.601)
  crypto.rs       ChaCha20-Poly1305 EncryptedWriter/Reader (feature-gated)

crates/server/src/
  main.rs              CLI args, transport selection, session loop, smart encoding, keepalive
  capture_scrap.rs     ScrapCapture (impl FrameCapture, cross-platform)
  encode_h264.rs       OpenH264Encoder (impl FrameEncoder, CPU baseline)
  encode_zstd.rs       ZstdEncoder (impl Encoder, lossless tiles)
  input_injector.rs    enigo: mouse/keyboard injection + type_text for paste
  transport_tcp.rs     TCP: Plain/Encrypted sender/receiver, split via try_clone
  transport_quic.rs    QUIC: quinn, self-signed TLS, keep-alive
  transport_ws.rs      WebServerTransport: HTTP static + WS fallback + WebRTC orchestration
  transport_webrtc.rs  str0m run_loop, ActiveClient, WebRtcSender/Receiver, bounded channels
  bin/mock_server.rs   Animated H.264 frames without screen capture

crates/client/src/
  main.rs              winit ApplicationHandler, reconnect loop, transport selection
  display_winit.rs     softbuffer rendering, coordinate mapping, cursor overlay
  input_capture.rs     winit KeyCode → phantom KeyCode mapping
  decode_h264.rs       OpenH264Decoder (impl FrameDecoder)
  decode_zstd.rs       ZstdDecoder (impl Decoder)
  transport_tcp.rs     TCP client: Plain/Encrypted, split
  transport_quic.rs    QUIC client: quinn, skip cert verification

crates/web/src/
  lib.rs               WASM entry, setup_webrtc (POST /rtc), WebCodecs decode,
                       Canvas render, mouse/keyboard/scroll/paste input capture,
                       TileUpdate zstd decompress (ruzstd), DataChannel send/recv

crates/gpu/src/
  lib.rs               Module exports (pub mod cuda, nvenc, nvfbc, sys)
  dl.rs                Runtime dlopen/dlsym abstraction (no build-time NVIDIA dep)
  sys.rs               C FFI types: CUDA, NVENC (SDK 12.1), NVFBC (v1.8/1.9 compat)
  cuda.rs              CUDA driver API: context, memory, memcpy, primary context
  nvenc.rs             NvencEncoder (impl FrameEncoder): H.264 GPU encode via NVENC
  nvfbc.rs             NvfbcCapture (impl FrameCapture): GPU screen capture via NVFBC

crates/bench/src/
  main.rs              Encoder benchmark: OpenH264 vs NVENC × resolutions + NVFBC zero-copy

crates/server/web/
  index.html           Minimal HTML loader for WASM

Docker:
  Dockerfile           Multi-stage: rust:bookworm builder → debian:bookworm-slim runtime
  docker-entrypoint.sh XFCE desktop + phantom-server modes
  docker-compose.yml   server / server-web / server-quic / mock modes
```
