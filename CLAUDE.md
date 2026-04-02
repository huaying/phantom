# Phantom — Developer Guide for Claude

## What This Project Is

Phantom is a high-performance, open-source remote desktop built in Rust. Target: Parsec-class latency (~20-50ms) with DCV-class quality (pixel-perfect text), single binary deployment, browser + native access.

~17,000 lines Rust (across 7 crates), 21 tests, MIT license. Runs on Linux + Windows.

## Build Commands

```bash
cargo build --release                                    # native (WSS web transport)
cargo build --release --features webrtc                  # native + WebRTC DataChannel support
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
1. **DataChannel + WebCodecs** — same approach as Parsec/Zoom, bypasses jitter buffer (30-80ms saved)
2. **WSS fallback on same port** — WebSocket + WebCodecs as reliable fallback (validated by Helix at scale)
3. **Single binary** — web client embedded, no Docker/GStreamer/coturn needed
4. **Rust WASM code sharing** — one codebase for server + web client
5. **~17K lines** — vs KasmVNC 200K+, Neko 15K+, RustDesk 150K

### Key Lessons From Competitors
- **Parsec (BorgGames/streaming-client)**: uses DataChannel reliable+ordered for video, MSE decode. Only other production DataChannel video impl.
- **Zoom**: uses unreliable DataChannel for video, WASM decode. Validated at massive scale.
- **Neko/Selkies**: use WebRTC Media Track (RTP), not DataChannel. Browser handles decode. Simpler but 30-80ms jitter buffer.
- **Helix**: killed WebRTC entirely, uses WebSocket + WebCodecs. Reports 20-30ms lower latency than their WebRTC setup.
- **Sunshine**: no web client. Custom UDP + RTP + Reed-Solomon FEC for native Moonlight clients.
- **DCV**: QUIC transport (we have it), GPU sharing (future)
- **KasmVNC**: per-rectangle quality tracking, multi-encoder mixing, DLP features
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

**CRITICAL: str0m SCTP cannot deliver messages >16KB reliably.** Regardless of reliable/ordered settings, large DataChannel messages (e.g. 70KB H.264 keyframe) silently fail. Root cause unknown (possibly SCTP congestion window, internal buffer limits, or fragmentation bugs). Workaround: application-level chunking — split messages >16KB into chunks with `[u32 total_len LE][payload]` header, reassemble on client via `ChunkAssembler`.

### Always H.264 full frames (tile mode removed)
Tile-based rendering (zstd per-tile) was removed — caused visual tearing when mixed with H.264 over high latency. Now every frame change triggers a full H.264 encode. TileDiffer still used to detect whether the screen changed (skips encode on static frames).

### Periodic keyframes (2s interval)
Server forces IDR keyframe every 2 seconds. Recovers from:
- WebRTC DataChannel packet loss (unreliable mode future)
- Client decoder errors
- Browser tab backgrounding/foregrounding

### Dual web transport: WSS default + WebRTC optional
- **WSS** (default): WebSocket upgrade on same HTTPS port 9900. No message size limits. Reliable. Validated by Helix as production-viable.
- **WebRTC DataChannel** (`--features webrtc` build flag, `?rtc` URL param): POST /rtc signaling, str0m 0.18, reliable+ordered. Needs chunking for messages >16KB (SCTP limitation). Only needed for future NAT traversal.
- **Native**: raw QUIC (no browser overhead, 15-30ms target)
- All produce same `Box<dyn MessageSender/Receiver>` → same session loop

---

## Key Implementation Details

### WebRTC run_loop (str0m official pattern)
- **One UDP socket** for entire server lifetime (never rebind — this was a hard bug)
- **One `run_loop` thread** managing one active client at a time
- **1ms UDP socket timeout** for responsive polling (was 50ms — caused visible lag)
- **poll_output after drain_outgoing** — flush written data immediately
- New POST /rtc → drain all pending Rtc, keep latest → replace active client immediately
- Session delivered via `Mutex<Option>` slot (always latest, stale auto-dropped)
- Bounded `sync_channel(30)` for video with `try_send` (backpressure, no blocking)
- **Chunking**: messages >16KB split into chunks before `ch.write()`. Client reassembles.

### Session reconnect (hard-won bugs)
These bugs took significant debugging. Don't reintroduce them:

1. **recv_msg() infinite spin**: MUST detect `mpsc::TryRecvError::Disconnected` and return error. Otherwise receive_loop thread spins forever, session never ends, no reconnect.

2. **Hello ordering**: Hello MUST go through video DC (same as VideoFrame). Control DC may deliver slower → decoder not configured when keyframe arrives → "Key frame is required" error.

3. **UDP socket lifecycle**: Do NOT create one socket per session. One socket for the whole server. str0m run_loop manages it. Old approach (bind/rebind per session) caused port conflicts.

4. **force_keyframe at session start**: New client needs IDR frame. Call `video_encoder.force_keyframe()` + `differ.reset()` at the top of `run_session()`.

5. **Web client got_keyframe guard**: WebCodecs throws if first frame is delta. Client skips all delta frames until first IDR arrives. Handles race condition where P-frames arrive before keyframe.

### Encoding flow
```
capture → TileDiffer (64x64 blocks) → any dirty?
  if dirty → H.264 full frame (VideoFrame)
  if static → skip encode (zero CPU)
  every 2s → force keyframe (IDR)
```
TileDiffer detects changes. If nothing changed, no encode. Hidden remote cursor means mouse movement alone = 0 dirty tiles = 0 CPU.

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

**Windows benchmark** (L40 GPU, 1080p, driver 537):
```
OpenH264 (CPU capture):      6-8 fps
NVENC (CPU capture+upload):  17-18 fps
DXGI→NVENC (zero-copy):     30-47 fps (limited by 52Hz refresh rate)
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
- **NVENC profile**: must use Baseline profile. OpenH264 decoder doesn't support High profile (NVENC default).
- **NVENC FORCEIDR**: value is 2 (0x2), not 4. Wrong value = keyframe never sent = client black screen.
- **Client VideoFrame decode**: must decode ALL frames sequentially, not just the last one. Keyframes get overwritten by empty P-frames in the channel buffer when encoder is fast (GPU).
- **Tile + H.264 mixed rendering**: caused visual tearing. Removed — always use H.264 full frames. Tile code still in codebase but unused.
- **HTTPS required for WebCodecs**: non-localhost HTTP is not a secure context. Server uses self-signed TLS (rcgen) for HTTPS.
- **str0m DataChannel >16KB**: SCTP silently drops large messages. MUST chunk into ≤16KB pieces. Chrome limit is 256KB but str0m fails well below that.
- **WebRTC session zombie**: after ICE disconnect, `send_msg()` swallows Full errors. Session never ends. Must detect and terminate.
- **WSS same port**: WS upgrade on HTTPS port 9900 (not separate port). Avoids self-signed cert rejection for second port.
- **HTTP query string**: strip `?ws` from path before routing, otherwise `/?ws` returns 404.
- **DXGI AcquireNextFrame timeout**: must use blocking timeout (e.g. 33ms), NOT 0. With timeout=0, capture loop misses frames between polls → 15fps instead of 30+fps.
- **DXGI refresh rate**: capture FPS capped by monitor refresh rate (DWM). RDP/headless may have low refresh (15-30Hz). Check with `wmic path Win32_VideoController get CurrentRefreshRate`.
- **WS disconnect under high bandwidth**: TLS write can exceed read timeout → tungstenite interprets as error. Increased timeout from 5ms to 50ms.
- **Stuck modifier keys**: Super/Meta (macOS Cmd) gets stuck on server after Cmd+Tab. Server releases all modifiers on session start. Client does NOT send Super/Meta, releases modifiers on focus loss.
- **NVENC reconnect black screen**: must recreate encoder between sessions. NVENC only outputs SPS/PPS on first encode after `nvEncInitializeEncoder()`. `force_keyframe()` produces IDR without SPS/PPS → WebCodecs can't decode. Server recreates encoder via `create_encoder()` after each session.
- **NVENC WebCodecs codec string**: must use `avc1.42c028` (Baseline Level 4.0). NVENC outputs Level 4.0 for 1080p. Previous `avc1.42001f` (Level 3.1) silently rejected 1080p (exceeds level max 720p).
- **Stale xdotool processes**: bench code spawns `xdotool mousemove` loops. Always `pkill -f xdotool` after bench testing — leftover loops send random mouse coordinates causing phantom cursor drift.
- **GNOME input**: enigo (XTest) works on GNOME when no other processes interfere. Previous "GNOME broken" diagnosis was caused by stale xdotool processes, not Mutter.
- **WASM feature flag**: `--no-default-features` builds server without WASM (for GPU-only VMs without wasm-pack).

---

## Implemented Features (27)

| # | Feature |
|---|---------|
| 1 | H.264 encoding (OpenH264 CPU, `--encoder` plugin architecture) |
| 2 | H.264 full frames + lossless refinement after 2s idle (tile-based smart encoding removed — caused tearing) |
| 3 | Periodic keyframe (2s interval, recovers from loss/decoder errors) |
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
| 14 | Web client WebRTC (DataChannel + chunking, WebCodecs, Canvas, POST /rtc) |
| 15 | Web client WSS fallback (`?ws` URL param, same HTTPS port) |
| 16 | Hidden remote cursor (mouse move = 0 CPU) |
| 17 | Docker XFCE test environment |
| 18 | Mock server (test without screen capture) |
| 19 | Encoder plugin architecture (--encoder flag, Box<dyn FrameEncoder>) |
| 20 | **NVENC GPU encoding** (`--encoder nvenc`, runtime dlopen, no build-time CUDA dep) |
| 21 | **NVFBC GPU capture** (`--capture nvfbc`, zero-copy CUdeviceptr) |
| 22 | **NVFBC→NVENC zero-copy pipeline** (capture+encode ~4ms at 1080p on A40) |
| 23 | **Windows support** (DXGI capture, OpenH264/NVENC, enigo input) |
| 27 | **DXGI→NVENC zero-copy** (`--capture dxgi --encoder nvenc`, D3D11 texture, no CPU copy) |
| 24 | **Auto-start** (Windows: schtasks ONLOGON, Linux: systemd) |
| 25 | **Self-signed HTTPS** (rcgen, enables WebCodecs on non-localhost) |
| 26 | **WASM pkg in repo** (Windows builds without wasm-pack) |

---

## Roadmap

### Immediate
| Task | Impact | Notes |
|------|--------|-------|
| ~~NVENC GPU encoding~~ | ✅ done | encode 47ms→10ms (CPU path), 4ms (zero-copy) |
| ~~NVFBC GPU capture~~ | ✅ done | zero-copy CUdeviceptr, ~0.4ms capture |
| ~~Windows support~~ | ✅ done | DXGI capture, auto-start via schtasks |
| ~~Web client WSS fallback~~ | ✅ done | `?ws` URL param, same HTTPS port |
| ~~Fix WebRTC session disconnect detection~~ | ✅ done | ICE Disconnected → drop ActiveClient → session ends |
| ~~DXGI→NVENC zero-copy~~ | ✅ done | 6fps→47fps on Windows L40 |
| ~~Make WS default, WebRTC optional~~ | ✅ done | `--features webrtc` + `?rtc` |
| **Web client auto-reconnect** | UX | WS disconnects under high load (50ms timeout helps but not 100%). Browser should auto-retry. |
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
| ~~Make WS default, WebRTC optional~~ | ✅ done — WS default, `--features webrtc` + `?rtc` for WebRTC |
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

## Technical Debt / Known Bugs

| Item | Severity |
|------|----------|
| ~~WebRTC session zombie~~ — fixed: ICE Disconnected → drop ActiveClient → channels disconnect → session ends | ✅ Fixed |
| **str0m SCTP drops large messages** — `ch.write()` silently fails for >16KB messages. Chunking workaround in place but root cause unknown. | **High** |
| **Server single-session** — only one client at a time. New connections queue until current session ends. Need proper session replacement. | Medium |
| BGRA→YUV via `pixel_f32()` (slow per-pixel callback) | Medium |
| Client threads leak on reconnect (no JoinHandle tracking) | Medium |
| No graceful shutdown (Ctrl+C) | Low |
| HTTP handler threads unbounded (no pool) | Medium |
| WS IO loop 50ms read timeout (was 5ms, increased for stability) | Low |
| Web client no auto-reconnect on WS disconnect | Medium |
| Mock server lacks encryption/input | Low |
| Tile code still in codebase but unused (encode_zstd, TileUpdate messages) | Low |

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
  main.rs              CLI args, transport selection, session loop, H.264 encoding, keepalive,
                       periodic keyframe, auto-start (--install/--uninstall)
  capture_scrap.rs     ScrapCapture (impl FrameCapture, cross-platform, DXGI on Windows)
  encode_h264.rs       OpenH264Encoder (impl FrameEncoder, CPU baseline)
  encode_zstd.rs       ZstdEncoder (impl Encoder, lossless tiles — UNUSED, kept for future)
  input_injector.rs    enigo: mouse/keyboard injection + type_text for paste, modifier release
  transport_tcp.rs     TCP: Plain/Encrypted sender/receiver, split via try_clone
  transport_quic.rs    QUIC: quinn, self-signed TLS, keep-alive
  transport_ws.rs      WebServerTransport: HTTPS static + WSS upgrade (same port) + WebRTC POST /rtc
  transport_webrtc.rs  str0m 0.18 run_loop, ActiveClient, chunked writes (>16KB), 1ms polling
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
  lib.rs               WASM entry, setup_webrtc (POST /rtc) + setup_ws (?ws fallback),
                       ChunkAssembler (reassembles >16KB DataChannel messages),
                       WebCodecs decode, Canvas render, got_keyframe guard,
                       mouse/keyboard/scroll/paste input capture, h264_has_idr() NAL parser

crates/gpu/src/
  lib.rs               Module exports (cuda, nvenc, nvfbc[linux], dxgi[win], dxgi_nvenc[win])
  dl.rs                Runtime dlopen/dlsym abstraction (no build-time NVIDIA dep)
  sys.rs               C FFI types: CUDA, NVENC (SDK 12.1), NVFBC (v1.8/1.9 compat)
  cuda.rs              CUDA driver API: context, memory, memcpy, primary context
  nvenc.rs             NvencEncoder (impl FrameEncoder): H.264 GPU encode via NVENC
  nvfbc.rs             NvfbcCapture (impl FrameCapture): GPU screen capture via NVFBC
  dxgi.rs              DxgiCapture: DXGI Desktop Duplication → ID3D11Texture2D (Windows)
  dxgi_nvenc.rs        DxgiNvencPipeline: DXGI capture + NVENC encode zero-copy (Windows)

crates/bench/src/
  main.rs              Encoder benchmark: OpenH264 vs NVENC × resolutions + NVFBC zero-copy

crates/server/web/
  index.html           Minimal HTML loader for WASM

Docker:
  Dockerfile           Multi-stage: rust:bookworm builder → debian:bookworm-slim runtime
  docker-entrypoint.sh XFCE desktop + phantom-server modes
  docker-compose.yml   server / server-web / server-quic / mock modes
```
