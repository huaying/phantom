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

### Native Client

```
Client (any OS)                              Server (Linux/Windows)
┌────────────────────────┐  TCP/QUIC        ┌──────────────────────────┐
│                        │  + ChaCha20      │                          │
│ OpenH264 Decode (CPU)  │◄══════════════╗  │ scrap Screen Capture     │
│ zstd Tile Decode       │◄─TileUpdate───╫──│ OpenH264 H.264 Encode   │
│ Local Cursor Overlay   │               ║  │ zstd Tile Encode         │
│ winit + softbuffer     │               ║  │ 64x64 TileDiffer         │
│ Auto Reconnect         │               ║  │ Two-Phase QualityState   │
│                        │═══════════════╝  │                          │
│ Input Capture (winit)  │──InputEvent─────►│ enigo Input Injection    │
│ Clipboard (arboard)    │◄─ClipboardSync──►│ Clipboard (arboard)      │
│ Ctrl+V Paste           │──PasteText──────►│ enigo.text() type-out    │
└────────────────────────┘                  └──────────────────────────┘
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

### Data Flow

```
Server main loop:
  1. capture frame (scrap)
  2. has_changes? (sample 64 points, or force after input injection)
  3. H.264 encode (openh264, BGRA→YUV→NAL)
  4. send VideoFrame over transport (TCP/QUIC/WebRTC DataChannel)
  5. if static > 2s: send TileUpdate (zstd lossless, all tiles)
  6. process input events every 1ms (during frame-pacing sleep)

Native client main loop (winit event-driven):
  1. recv messages (network thread → channel)
  2. decode VideoFrame (openh264) → update_full_frame
  3. decode TileUpdate (zstd) → update_tiles (overlay)
  4. draw local cursor at mouse position
  5. present (softbuffer, scaled to window)
  6. winit events → InputEvent / PasteText / ClipboardSync
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

| Trait | Current Impl | Future | Purpose |
|-------|-------------|--------|---------|
| `FrameCapture` | `ScrapCapture` | DMA-BUF, NVFBC | Screen capture |
| `FrameEncoder` | `OpenH264Encoder` | NVENC, VAAPI, x264 | Video encoding |
| `FrameDecoder` | `OpenH264Decoder` | — | Video decoding |
| `Encoder` (tile) | `ZstdEncoder` | — | Lossless tile encoding |
| `Decoder` (tile) | `ZstdDecoder` | — | Lossless tile decoding |
| `MessageSender` | Plain/Enc/Quic/WS | WebRTC DataChannel | Send messages |
| `MessageReceiver` | Plain/Enc/Quic/WS | WebRTC DataChannel | Receive messages |

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
| 14 | **WASM client crate** | 97KB, shares phantom-core, WebCodecs+Canvas+Input |
| 15 | **Docker test env** | XFCE desktop, OrbStack, 1920x1080 |
| 16 | **Mock server** | Animated H.264 test frames, no screen capture needed |

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

## Web Client Plan

### Phase 1: WebSocket (get it working)
WebSocket as data transport. Simplest path to browser access.

- `crates/server/src/web_server.rs` — axum HTTP (embedded static files + WS)
- `crates/server/src/transport_ws.rs` — WS MessageSender/Receiver
- `crates/server/web/index.html` — minimal HTML loader
- WASM output embedded in server binary via `include_bytes!`
- `--transport web` flag

### Phase 2: WebRTC DataChannel (upgrade to UDP)
Replace WS data path with DataChannel. WS remains for signaling only.

```
DataChannel #1 — Video:     ordered=false, maxRetransmits=0 (like raw UDP)
DataChannel #2 — Input:     ordered=true,  maxRetransmits=2
DataChannel #3 — Control:   ordered=true,  reliable
```

- `crates/server/src/transport_webrtc.rs` — str0m/webrtc-rs
- WASM client: RTCPeerConnection via web-sys, fallback to WS
- No HTTPS required (signaling over ws://, data over DTLS)

### Phase 3: Multi-stream + Lossless in Browser
- TileUpdate over reliable DataChannel → zstd decompress in WASM
- Two-phase rendering in browser: H.264 + lossless overlay

---

## Roadmap

### Performance
| Task | Impact | Status |
|------|--------|--------|
| NVENC GPU encoding | encode 15ms→2ms | Planned (need GPU) |
| VAAPI GPU encoding | AMD/Intel GPU | Planned |
| x264 via FFmpeg | 2-3x better compression | Planned |
| AV1 encoding | 30% better than H.264 | Planned |
| SIMD color conversion | 4x faster YUV↔RGB | Planned |
| Web client (WebSocket) | Browser access | **In progress** |
| Web client (WebRTC DC) | 20-50ms in browser | Planned |

### Features
| Task | Impact | Status |
|------|--------|--------|
| Audio forwarding | Meetings, media | Planned |
| Wayland capture | Modern Linux | Planned |
| Multi-monitor | Dev setups | Planned |
| File transfer | Drag-and-drop | Planned |
| NAT traversal | Firewall bypass | Planned |

### Enterprise
| Task | Impact | Status |
|------|--------|--------|
| GPU sharing | Cloud workstations | Planned |
| DLP | Watermark, clipboard control | Planned |
| Session recording | Audit | Planned |

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
| OpenH264 | Zero system deps, BSD license. Swappable via FrameEncoder trait |
| ChaCha20 over TLS | No cert management, works with TCP split |
| winit + softbuffer | OS-native key repeat/modifiers, proper event loop |
| Two-phase rendering | DCV's core insight: lossy for motion, lossless for reading |
| WebRTC DataChannel (planned) | No jitter buffer (30-80ms savings vs media track) |
| Rust WASM (not JS) | Share phantom-core code, one language, near-native perf |
| 64x64 tiles | Balance between diff granularity and overhead |
| Session random nonce | Prevent nonce reuse across connections with same key |

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
