# Phantom Remote Desktop — Design Document

## Vision

A high-performance, open-source remote desktop built in Rust. Target: Parsec-class latency (~20-50ms) with DCV-class quality (pixel-perfect text), single binary deployment, browser and native access.

---

## Competitive Position

```
                    Latency     Text Quality    Deploy        Web Client  Open Source
                    ────────    ────────────    ──────        ──────────  ──────────
Parsec              15-30ms     lossy(blurry)   simple        ❌          ❌
NICE DCV            30-60ms     pixel-perfect   medium        ✅(limited) ❌
KasmVNC             80-150ms    pixel-perfect   Docker        ✅          ✅
Neko                80-150ms    lossy(blurry)   Docker        ✅          ✅
Selkies (Google)    70-120ms    lossy(blurry)   complex       ✅          ✅
RustDesk            50-100ms    lossy(blurry)   simple        ✅(beta)    ✅
────────────────────────────────────────────────────────────────────────────────
Phantom (target)    20-50ms     pixel-perfect   single binary ✅          ✅
```

### Phantom's Unique Advantages

**1. Two-Phase Rendering** — no open-source competitor has this
- Motion → H.264 lossy (low latency) → Static 2s → zstd lossless (pixel-perfect)
- Neko/Selkies: always lossy, text always blurry
- KasmVNC: similar concept but JPEG/WebP-based, not H.264
- DCV: has it (but proprietary)

**2. DataChannel + WebCodecs** (planned) — faster than all WebRTC web clients
- Neko/Selkies/CloudRetro: WebRTC Media Track → jitter buffer adds 30-80ms
- Phantom: DataChannel (unreliable) + WebCodecs → zero jitter buffer
- Measured: theirs 80-150ms, ours target 20-50ms

**3. Single Binary, Zero Dependencies**
- KasmVNC: needs X server + Docker
- Neko: needs Docker + GStreamer + Pion
- Selkies: needs GStreamer + coturn + signaling server
- Phantom: one binary, web client embedded. Just run it.

**4. Rust WASM Code Sharing**
- Other projects: Server (C++/Go) + Web client (JS) = two codebases
- Phantom: phantom-core compiles to native + WASM = one codebase

**5. Minimal Codebase**
- Phantom: ~2,500 lines Rust
- KasmVNC: 200K+ C++
- Neko: 15K+ Go
- RustDesk: 150K Rust

---

## Architecture

### Host Engine — Hardware-Adaptive Pipeline

```
┌─────────────────────────────────────────────────────────────┐
│                    Hardware Probe (startup)                   │
│  Detect: GPU model (NVENC?) / CPU cores / OS / display       │
│  Select: capture method + encoder + transport capabilities   │
└──────────────────────┬──────────────────────────────────────┘
                       ▼
┌─────────────────────────────────────────────────────────────┐
│               Capture + Smart Encode Pipeline                │
│                                                              │
│  ┌──────────┐    ┌───────────┐    ┌────────────────────┐    │
│  │ Capture   │    │ TileDiffer│    │ Encoding Decision  │    │
│  │           │───►│ 64x64     │───►│                    │    │
│  │ GPU mode: │    │ blocks    │    │ <10% dirty:        │    │
│  │  NVFBC    │    │           │    │   → zstd tiles only│    │
│  │  DMA-BUF  │    │ Tracks:   │    │   (0.1ms, CPU-lite)│    │
│  │           │    │  dirty %  │    │                    │    │
│  │ CPU mode: │    │  dirty    │    │ ≥10% dirty:        │    │
│  │  scrap    │    │  regions  │    │   → H.264 full frame│   │
│  │  DXGI     │    │           │    │   (15ms CPU/2ms GPU)│   │
│  └──────────┘    └───────────┘    └────────────────────┘    │
│                                                              │
│  Encoder backends (auto-detect, --encoder override):         │
│    GPU: NVENC (H.264/AV1) → VAAPI (H.264) → fallback       │
│    CPU: x264 (H.264) → OpenH264 (H.264) → zstd-only mode   │
└─────────────────────────────────────────────────────────────┘
```

Key insight: **smart encoding decision based on dirty area size**.
On CPU-only hosts, 90% of updates are small (typing, cursor, notifications).
Sending only dirty tiles with zstd costs 0.1ms vs 15-30ms for full-frame H.264.
This reduces CPU usage from ~80% to ~5% for typical office work on a 2-core VM.

### Dual-Track Network Layer

```
┌─────────────────────────────────────────────────────────────┐
│              Protocol Multiplexing (same port 9900)           │
│                                                              │
│  Native App connects → auto-detect → QUIC/UDP track         │
│  Browser connects    → auto-detect → WebRTC/WS track        │
│                                                              │
│  ┌─────────────────────────┐  ┌───────────────────────────┐ │
│  │ Track A: Native QUIC    │  │ Track B: Web Client       │ │
│  │ (15-30ms target)        │  │ (20-50ms target)          │ │
│  │                         │  │                           │ │
│  │ Unreliable Datagram:    │  │ DataChannel #1 (video):   │ │
│  │   H.264/AV1 video ────► │  │   unreliable, unordered ►│ │
│  │                         │  │                           │ │
│  │ Reliable Stream:        │  │ DataChannel #2 (input):   │ │
│  │   Input, Control  ◄───► │  │   ordered, maxRetrans=2  │ │
│  │                         │  │                           │ │
│  │ 0-RTT reconnect         │  │ DataChannel #3 (control): │ │
│  │ ChaCha20 or TLS         │  │   reliable               │ │
│  │ No browser overhead     │  │                           │ │
│  │                         │  │ WebSocket: signaling only │ │
│  │                         │  │ DTLS encryption (auto)    │ │
│  │                         │  │ ICE/STUN NAT traversal    │ │
│  └─────────────────────────┘  └───────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

Why two tracks instead of one protocol:
- Native app has no browser sandbox → can use raw QUIC datagrams (lowest possible latency)
- Browser is sandboxed → must use WebRTC (but DataChannel avoids jitter buffer)
- Both tracks produce the same `MessageSender`/`MessageReceiver` → same session loop

### Native Client (current)

```
Client (any OS)                              Server (Linux/Windows)
┌────────────────────────┐  QUIC/TCP        ┌──────────────────────────┐
│                        │  + ChaCha20      │                          │
│ OpenH264 Decode (CPU)  │◄══════════════╗  │ scrap Screen Capture     │
│ zstd Tile Decode       │◄─TileUpdate───╫──│ Smart Encode Pipeline    │
│ Local Cursor Overlay   │               ║  │ (H.264 or zstd tiles)    │
│ winit + softbuffer     │               ║  │ Two-Phase QualityState   │
│ Auto Reconnect         │               ║  │                          │
│                        │═══════════════╝  │                          │
│ Input Capture (winit)  │──InputEvent─────►│ enigo Input Injection    │
│ Clipboard (arboard)    │◄─ClipboardSync──►│ Clipboard (arboard)      │
│ Ctrl+V Paste           │──PasteText──────►│ enigo.text() type-out    │
└────────────────────────┘                  └──────────────────────────┘

Future: Hardware decode (DXVA2/VideoToolbox/VA-API) + GPU render (wgpu)
```

### Native Client — Target Architecture (v2)

```
Client (any OS)
┌────────────────────────────────────────┐
│ QUIC Unreliable Datagram → NAL units   │
│    ↓                                   │
│ Hardware Decode (zero-copy):           │
│   Windows: DXVA2 / D3D11VA            │
│   macOS:   VideoToolbox               │
│   Linux:   VA-API                     │
│    ↓                                   │
│ GPU Direct Render (frame stays in VRAM)│
│   wgpu / Vulkan / Metal               │
│    ↓                                   │
│ Present (vsync)                        │
│                                        │
│ Input: Raw Input API (1000Hz polling)  │
│ 0-RTT reconnect on network switch      │
└────────────────────────────────────────┘
```

### Web Client (planned)

```
Browser                                    Server (port 9900)
┌──────────────────────┐                   ┌─────────────────────┐
│ phantom_web.wasm     │                   │ TCP:                │
│ (97KB, phantom-core) │  DataChannel #1   │  GET / → HTML+WASM  │
│                      │◄═══(UDP)═════════╪═ H.264 frames       │
│ WebCodecs decode     │  unreliable       │  (axum static serve)│
│ Canvas render        │  unordered        │                     │
│                      │                   │  GET /ws → signaling│
│ Input capture ═══════╪═══(UDP)══════════╪═► enigo inject      │
│ bincode serialize    │  DataChannel #2   │                     │
│                      │  ordered          │ UDP:                │
│                      │  maxRetransmits=2 │  WebRTC endpoint    │
│                      │                   │  (str0m / webrtc-rs)│
│ Clipboard/Paste ◄═══╪═══(reliable)═════╪═► DataChannel #3    │
│                      │                   │                     │
│ WebSocket            │                   │                     │
│ (signaling only) ◄───╪──(TCP)───────────╪─ SDP/ICE exchange  │
└──────────────────────┘                   └─────────────────────┘
```

Why DataChannel + WebCodecs instead of WebRTC Media Track:
- Media Track adds 30-80ms jitter buffer (designed for video calls, not remote desktop)
- DataChannel delivers raw bytes instantly → WebCodecs GPU decode → Canvas
- Measured: 20-50ms vs 80-150ms end-to-end

### Data Flow — Three-Phase Encoding

```
Server main loop:
  1. capture frame (scrap / NVFBC / DMA-BUF)
  2. TileDiffer: which 64x64 tiles changed? how many?
  3. ENCODING DECISION:
     ├── dirty < 10% → zstd tiles only (0.1ms, CPU-lite)     ← typing, cursor
     ├── dirty ≥ 10% → H.264 full frame (15ms CPU / 2ms GPU) ← scrolling, video
     └── static > 2s → zstd lossless ALL tiles (pixel-perfect) ← quality refinement
  4. send over transport (TCP/QUIC/WebRTC DataChannel)
  5. process input events every 1ms (during frame-pacing sleep)

Native client main loop (winit event-driven):
  1. recv messages (network thread → channel)
  2. decode VideoFrame (openh264 / HW decode) → update_full_frame
  3. decode TileUpdate (zstd) → update_tiles (overlay)
  4. draw local cursor at mouse position
  5. present (softbuffer / wgpu)
  6. winit events → InputEvent / PasteText / ClipboardSync

CPU budget on a 2-core VM (typical office work):
  Without smart encoding:  60-100% CPU (every keystroke = full H.264)
  With smart encoding:     5-10% CPU  (keystrokes = tiny zstd tiles)
```

### Wire Protocol

```
Framing (TCP, unencrypted):  [4B length][bincode payload]
Framing (TCP, encrypted):    [4B length][12B nonce][ciphertext + 16B tag]
Framing (QUIC):              [4B length][bincode payload] (TLS built-in)
Framing (WebSocket):         [bincode payload] (WS has built-in framing)
Framing (DataChannel):       [4B length][bincode payload] (byte stream)

Messages:
  Hello          server→client   {width, height, format}
  VideoFrame     server→client   {sequence, EncodedFrame{codec, data, is_keyframe}}
  TileUpdate     server→client   {sequence, Vec<EncodedTile>}
  Input          client→server   {MouseMove|MouseButton|MouseScroll|Key}
  ClipboardSync  bidirectional   String
  PasteText      client→server   String (server types it out via enigo)
  Ping/Pong      bidirectional
```

### Crate Structure

```
phantom/
├── crates/
│   ├── core/        ~600 lines   Traits, protocol, tile differ, color, crypto, clipboard
│   ├── server/      ~550 lines   Capture, H.264 encode, input inject, pipeline, transports
│   ├── client/      ~500 lines   H.264 decode, winit display, input capture, reconnect
│   └── web/         ~350 lines   WASM client (WebCodecs, Canvas, input, clipboard)
├── tests            ~250 lines   21 tests + e2e
├── Docker                        XFCE desktop test environment
└── total           ~2,500 lines
```

### Trait Abstractions (swappable components)

| Trait | Current Impl | Future Impls | Purpose |
|-------|-------------|--------------|---------|
| `FrameCapture` | `ScrapCapture` | NVFBC (GPU zero-copy), DMA-BUF/KMS | Screen capture |
| `FrameEncoder` | `OpenH264Encoder` | NVENC, VAAPI, x264 (auto-detect) | Video encoding |
| `FrameDecoder` | `OpenH264Decoder` | DXVA2, VideoToolbox, VA-API | Video decoding (native) |
| `Encoder` (tile) | `ZstdEncoder` | — | Lossless tile encoding |
| `Decoder` (tile) | `ZstdDecoder` | — | Lossless tile decoding |
| `MessageSender` | Plain/Enc/Quic | WS, WebRTC DC, QUIC Datagram | Send messages |
| `MessageReceiver` | Plain/Enc/Quic | WS, WebRTC DC, QUIC Datagram | Receive messages |

Hardware probe at startup auto-selects the best implementation for each trait.

---

## Implemented Features (v0.1)

| # | Feature | Details |
|---|---------|---------|
| 1 | **H.264 encoding** | OpenH264 Baseline, CPU. `--encoder` flag for future GPU backends |
| 2 | **Two-phase rendering** | H.264 lossy → static 2s → zstd pixel-perfect tile update |
| 3 | **Tile-based dirty detection** | 64x64 blocks, sampling fast-path, force-encode after input |
| 4 | **ChaCha20-Poly1305 encryption** | 256-bit key, session random nonce prefix, auto-gen key |
| 5 | **QUIC/UDP transport** | quinn, self-signed TLS, keep-alive, `--transport quic` |
| 6 | **TCP transport** | With optional ChaCha20, `--transport tcp` |
| 7 | **Clipboard sync** | Bidirectional via arboard, 250ms polling, echo-loop prevention |
| 8 | **Ctrl+V paste** | Client intercepts → PasteText → server enigo.text() |
| 9 | **Auto-reconnect** | Exponential backoff 500ms→10s, window persists |
| 10 | **Local cursor** | 12x19 arrow bitmap overlay, zero-latency feel |
| 11 | **Window scaling** | Auto-fit 80% screen, resize, coordinate mapping |
| 12 | **Adaptive quality** | Congestion-based frame skipping (1/2→1/3→1/4) |
| 13 | **Native client (winit)** | OS key repeat, proper modifiers, event-driven |
| 14 | **Web client (WebRTC)** | 207KB WASM, DataChannel (UDP), WebCodecs+Canvas, POST /rtc signaling |
| 15 | **Web client (WS fallback)** | WebSocket transport preserved for adaptive mode |
| 16 | **Smart encoding** | dirty <10% → tiles (0.1ms), ≥10% → H.264 (15ms), saves 90% CPU |
| 17 | **Hidden remote cursor** | Server hides OS cursor, client renders locally, mouse move = 0 CPU |
| 18 | **Docker test env** | XFCE desktop, OrbStack, 1920x1080 |
| 19 | **Mock server** | Animated H.264 test frames, no screen capture needed |

### Test Coverage (21 tests)

| Category | Tests | What They Verify |
|----------|-------|-----------------|
| Tile differ | 5 | First dirty, identical skip, single pixel, edge tiles, data |
| Color conversion | 2 | BGRA↔YUV roundtrip (white, black) |
| Crypto | 3 | Encrypt/decrypt roundtrip, wrong key, key parse |
| H.264 | 2 | Encode/decode roundtrip, P-frame < keyframe |
| Protocol | 1 | Serialize/deserialize all message types |
| Zstd | 2 | Tile roundtrip, solid color >100x compression |
| Clipboard | 2 | Echo loop prevention, duplicate remote ignored |
| E2E headless | 2 | 10-frame H.264 over TCP, encrypted Hello+Clipboard |

---

## Web Client — Implemented

### WebRTC DataChannel (current)

Signaling via HTTP POST `/rtc` (str0m pattern — no WebSocket for signaling):
```
Browser: createOffer → POST /rtc → setRemoteDescription(answer)
Server:  str0m accept_offer → return answer → run_loop drives ICE/DTLS
```

3 DataChannels:
- **Video DC** (reliable, ordered): Hello + VideoFrame + TileUpdate
- **Input DC** (ordered, maxRetransmits=2): Mouse/keyboard events
- **Control DC** (reliable): Clipboard, Ping

Server architecture (str0m official pattern):
- Single UDP socket, single `run_loop` thread, one active client
- `ActiveClient` replaced on browser refresh (immediate, no timeout)
- Session delivered via `Mutex<Option>` slot (always latest, stale auto-dropped)
- Bounded channels for video (30 frames) prevent memory growth
- Keepalive ping every 1s detects dead sessions

WebSocket fallback preserved (`accept_ws()` ready for future adaptive mode).

### WASM Client (207KB)
- Shares phantom-core (protocol, input, clipboard types)
- WebCodecs H.264 hardware decode → Canvas drawImage
- TileUpdate: ruzstd decompress → BGRA→RGBA → putImageData
- RTCPeerConnection + 3 DataChannels via web-sys
- Ctrl+V paste interception

---

## Roadmap

### Immediate (next up)
| Task | Impact | Effort |
|------|--------|--------|
| **Smart encoding (dirty% threshold)** | CPU-only hosts: 80%→5% CPU | **~20 lines** |
| **Web client Phase 1 (WebSocket)** | Browser access | Medium |
| **Hardware probe (auto-detect GPU)** | Auto-select best encoder | Low |

### Host Performance
| Task | Impact | Effort |
|------|--------|--------|
| NVENC GPU encoding | encode 15ms→2ms | High (need GPU) |
| NVFBC GPU capture | zero-copy from VRAM | High (NVIDIA only) |
| VAAPI GPU encoding | AMD/Intel GPU | Medium |
| x264 via FFmpeg | 2-3x better compression | Medium |
| AV1 encoding (NVENC/SVT-AV1) | 30% better than H.264 | Medium |
| DMA-BUF/KMS capture | Linux zero-copy | Medium |
| SIMD color conversion | 4x faster YUV↔RGB | Low |

### Native Client Performance
| Task | Impact | Effort |
|------|--------|--------|
| QUIC Unreliable Datagram | video over datagram, no retransmit | Medium |
| 0-RTT reconnect | instant reconnect on network switch | Low (quinn supports) |
| Hardware decode (DXVA2/VT/VA-API) | decode 10ms→1ms | High |
| GPU direct render (wgpu) | zero-copy display | High |
| Raw Input 1000Hz polling | gaming-grade input | Medium (Windows) |

### Web Client
| Task | Impact | Effort |
|------|--------|--------|
| Phase 1: WebSocket transport | Browser access works | Medium |
| Phase 2: WebRTC DataChannel | 80ms→20ms browser latency | High |
| Phase 3: Lossless tiles in browser | pixel-perfect text in browser | Medium |

### Features
| Task | Impact | Effort |
|------|--------|--------|
| Audio forwarding (Opus) | Meetings, media | High |
| Wayland capture (PipeWire) | Modern Linux | High |
| Multi-monitor | Dev setups | Medium |
| File transfer | Drag-and-drop | Medium |
| NAT traversal (STUN/TURN) | Firewall bypass | Medium |

### Enterprise
| Task | Impact | Effort |
|------|--------|--------|
| GPU sharing (OpenGL interposition) | Cloud workstations | Very High |
| DLP (watermark, clipboard control) | Enterprise security | Medium |
| Session recording | Audit/training | Medium |
| Protocol multiplexing | Same port, auto-detect client type | Medium |

---

## Technical Debt

| Item | Severity | Status |
|------|----------|--------|
| BGRA→YUV via `pixel_f32()` (slow, per-pixel callback) | Medium | Open |
| Client threads leak on reconnect (no JoinHandle tracking) | Medium | Open |
| No graceful shutdown (Ctrl+C) | Low | Open |
| `has_changes()` sampling can miss small changes | Low | Mitigated (force after input) |
| Mock server lacks encryption/input support | Low | Open |
| `pipeline_test.rs` unused import | Trivial | Open |

---

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| Rust | Memory safety, performance, WASM target, trait abstraction |
| OpenH264 default | Zero system deps, BSD license. Swappable via FrameEncoder trait |
| No GStreamer | Direct function calls = 0ms pipeline overhead. Sunshine/Parsec don't use it either. Our pipeline is 3 steps, not 20. |
| Smart encoding (dirty% threshold) | 90% of office work = small updates. zstd tiles cost 0.1ms vs H.264 15ms. CPU-only hosts need this. |
| Three-phase rendering | Phase 1: zstd tiles (small changes). Phase 2: H.264 (large changes). Phase 3: zstd lossless (quality refinement) |
| WebRTC DataChannel for web (not Media Track) | No jitter buffer = 30-80ms savings. Measured: 20-50ms vs 80-150ms |
| WebRTC over WebTransport for web | WebTransport requires HTTPS + certs (pure IP doesn't work). WebRTC works with any IP, has NAT traversal |
| QUIC Datagram for native (planned) | Even lower than reliable stream — video packets never retransmitted, next frame replaces lost one |
| Dual-track network | Native app = raw QUIC (no browser overhead, 15ms). Browser = WebRTC DC (sandboxed but 20-50ms). Same session loop. |
| Hardware auto-detect | Probe GPU at startup, auto-select best encoder/capture. No manual --encoder needed. |
| Rust WASM (not JS/TS) | Share phantom-core code, one language, bincode works in WASM |
| ChaCha20 for TCP, TLS for QUIC, DTLS for WebRTC | Each transport uses its natural encryption. No redundant layers. |
| 64x64 tiles | Balance between diff granularity and overhead |

---

## Usage

```bash
# Build
cargo build --release

# Server (auto-generates encryption key)
cargo run --release -p phantom-server
# → prints: --key <64 hex chars>

# Native client
cargo run --release -p phantom-client -- -c <ip>:9900 --key <hex>

# QUIC mode (better for WAN)
cargo run --release -p phantom-server -- --transport quic
cargo run --release -p phantom-client -- --transport quic -c <ip>:9900

# No encryption (testing only)
cargo run --release -p phantom-server -- --no-encrypt
cargo run --release -p phantom-client -- -c 127.0.0.1:9900 --no-encrypt

# Web client (planned)
cargo run --release -p phantom-server -- --transport web
# → open http://localhost:9900 in browser

# Docker test environment
docker build -t phantom .
docker run --rm -p 9900:9900 phantom server

# Build WASM client
wasm-pack build crates/web --target web

# Run tests
cargo test
```
