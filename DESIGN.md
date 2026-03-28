# Phantom Remote Desktop — Design Document

## Vision

A lightweight, extensible remote desktop built in Rust. Optimized for controlling real machines (not virtual desktops) with DCV-class quality — pixel-perfect text after motion stops, low latency mouse feel, encrypted by default.

---

## Current Architecture

```
Client (any OS)                              Server (Linux/Windows)
┌────────────────────────┐  TCP + ChaCha20  ┌──────────────────────────┐
│                        │◄════════════════╗│                          │
│ OpenH264 Decode (CPU)  │  VideoFrame     ║│ scrap Screen Capture     │
│ zstd Tile Decode       │◄─TileUpdate─────╫│ OpenH264 H.264 Encode   │
│ Local Cursor Overlay   │                 ║│ zstd Tile Encode         │
│ minifb Window (scaled) │                 ║│ 64x64 TileDiffer         │
│ Auto Reconnect         │                 ║│ Two-Phase QualityState   │
│                        │════════════════╝│                          │
│ Input Capture (minifb) │──InputEvent────►│ enigo Input Injection    │
└────────────────────────┘                 └──────────────────────────┘
```

### Data Flow

```
Server main loop:
  1. capture frame (scrap)
  2. has_changes? (sample 64 points) → skip if identical
  3. H.264 encode (openh264, BGRA→YUV→NAL)
  4. send VideoFrame over encrypted TCP
  5. if static > 500ms: send TileUpdate (zstd lossless, all tiles)

Client main loop:
  1. recv messages (network thread → channel)
  2. decode VideoFrame (openh264) → update_full_frame
  3. decode TileUpdate (zstd) → update_tiles (overlay)
  4. draw local cursor at mouse position
  5. present (minifb, scaled to window)
  6. poll input → send InputEvent (input thread)
```

### Wire Protocol

```
Framing (unencrypted): [4B length][bincode payload]
Framing (encrypted):   [4B length][12B nonce][ciphertext + 16B tag]

Messages:
  Hello        server→client   {width, height, format}
  VideoFrame   server→client   {sequence, EncodedFrame{codec, data, is_keyframe}}
  TileUpdate   server→client   {sequence, Vec<EncodedTile>}
  Input        client→server   {MouseMove|MouseButton|MouseScroll|Key}
  Ping/Pong    bidirectional
```

### Crate Structure

```
phantom/
├── crates/
│   ├── core/           550 lines   Traits, protocol, tile differ, color, crypto
│   ├── server/         450 lines   Capture, H.264 encode, input inject, pipeline
│   └── client/         500 lines   H.264 decode, display, input capture, cursor
├── tests               200 lines   17 tests
└── total              ~1800 lines
```

### Trait Abstractions (swappable components)

| Trait | Current Impl | Purpose |
|-------|-------------|---------|
| `FrameCapture` | `ScrapCapture` | Screen capture |
| `FrameEncoder` | `OpenH264Encoder` | Full-frame video encoding |
| `FrameDecoder` | `OpenH264Decoder` | Full-frame video decoding |
| `Encoder` (tile) | `ZstdEncoder` | Tile-level lossless encoding |
| `Decoder` (tile) | `ZstdDecoder` | Tile-level lossless decoding |
| `Display` | `MinifbDisplay` | Client-side rendering |
| `Connection` | `TcpConnection` | Network transport |
| `MessageSender` | `PlainSender` / `EncSender` | Send half (plain or encrypted) |
| `MessageReceiver` | `PlainReceiver` / `EncReceiver` | Receive half (plain or encrypted) |

---

## Implemented Features

### v0.1 — Current State

| # | Feature | How It Works | Files |
|---|---------|-------------|-------|
| 1 | **H.264 encoding** | OpenH264 Baseline Profile, CPU, BGRA→YUV via RGBSource trait | `encode_h264.rs`, `decode_h264.rs` |
| 2 | **Two-phase rendering** | Motion → H.264 lossy 30fps. Static 500ms → zstd lossless tile update | `server/main.rs` QualityState |
| 3 | **Tile-based dirty detection** | 64x64 blocks, byte compare, sampling fast-path (64 points) | `core/tile.rs` |
| 4 | **ChaCha20-Poly1305 encryption** | 256-bit key, counter nonce, AEAD. Auto-gen key on server | `core/crypto.rs` |
| 5 | **Key-based authentication** | Wrong key → decrypt fail → connection rejected | `crypto.rs` nonce verification |
| 6 | **Client window scaling** | Auto-fit to 80% of screen, AspectRatioStretch, resize support | `display_minifb.rs` fit_to_screen |
| 7 | **Coordinate mapping** | Window-space mouse → server-space via scale factors | `display_minifb.rs` map_mouse |
| 8 | **Auto-reconnect** | Exponential backoff 500ms→10s, window stays open during reconnect | `client/main.rs` connect loop |
| 9 | **Local cursor rendering** | 12x19 arrow bitmap, drawn before present, save/restore pixels | `cursor.rs` |
| 10 | **Bidirectional input** | Mouse (move/click/scroll) + keyboard (full keymap) → enigo injection | `input_capture.rs`, `input_injector.rs` |
| 11 | **Mock server** | Animated H.264 frames without screen capture, for testing | `bin/mock_server.rs` |

### Test Coverage (17 tests)

| Category | Tests | What They Verify |
|----------|-------|-----------------|
| Tile differ | 5 | First frame all dirty, identical no dirty, single pixel change, edge tiles, data correctness |
| Color conversion | 2 | BGRA↔YUV roundtrip (white, black) |
| Crypto | 3 | Encrypt/decrypt roundtrip, wrong key fails, key hex parse |
| H.264 | 2 | Encode/decode roundtrip (solid red), P-frame < keyframe |
| Protocol | 1 | Serialize/deserialize all message types |
| Zstd | 2 | Tile roundtrip, solid color >100x compression |
| Pipeline | 1 | Full diff→encode→serialize→deserialize→decode |
| Change detection | 1 | Single pixel change detected |

---

## Competitive Landscape

### Feature Matrix

| Feature | Phantom | DCV | KasmVNC | Sunshine | RustDesk | TurboVNC |
|---------|---------|-----|---------|----------|----------|----------|
| H.264 encode | ✅ CPU | ✅ NVENC | ✅ FFmpeg/NVENC/VAAPI | ✅ NVENC zero-copy | ✅ | ❌ |
| H.265/AV1 | ❌ | ❌ | ✅ | ✅ | ✅ | ❌ |
| Lossless refinement | ✅ zstd tiles | ✅ pixel-perfect | ✅ per-rect refresh | ❌ | ❌ | ✅ native |
| Per-rect quality | ❌ whole-frame | ❌ | ✅ frequency-based | ❌ | ❌ | ❌ |
| GPU encode | ❌ | ✅ NVENC | ✅ NVENC+VAAPI | ✅ NVENC | ✅ | ❌ |
| GPU capture | ❌ | ✅ NvFBC | ❌ (is Xvnc) | ✅ NvFBC/DMA-BUF | ❌ | ✅ VirtualGL |
| QUIC/UDP | ❌ TCP | ✅ QUIC default | Experimental WebRTC | ❌ TCP | ✅ UDP P2P | ❌ |
| Encryption | ✅ ChaCha20 | ✅ TLS | ✅ TLS | ✅ AES-GCM | ✅ | ❌ |
| Web client | ❌ | ✅ | ✅ browser-only | ❌ | ✅ | ❌ |
| Native client | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| Audio | ❌ | ✅ 7.1 | ✅ PulseAudio | ✅ Opus | ✅ | ❌ |
| Clipboard | ❌ | ✅ | ✅ with DLP | ✅ | ✅ | ❌ |
| Multi-user | ❌ | ✅ virtual sessions | ✅ permissions | ❌ | ❌ | ❌ |
| Local cursor | ✅ | ✅ | ❌ | ✅ | ✅ | ❌ |
| Auto reconnect | ✅ | ❌ | ❌ | ❌ | ✅ | ❌ |
| Real desktop capture | ✅ | ✅ | ❌ (virtual only) | ✅ | ✅ | ✅ |
| Wayland | ❌ | ❌ | ❌ | ✅ wlroots | ✅ PipeWire | ❌ |
| Container-friendly | ❌ | ❌ | ✅ designed for it | ❌ | ❌ | ❌ |
| Codebase | 1.8K Rust | proprietary | 200K+ C++ | 80K C++ | 150K Rust | 200K C/Java |

### Key Lessons From Each Competitor

#### From DCV (the gold standard)
- **Two-phase rendering** — we already have this ✅
- **QUIC/UDP transport** — eliminates TCP head-of-line blocking on WAN
- **GPU sharing via OpenGL interposition** — multi-session on one GPU without vGPU
- **Bandwidth-first adaptation** — measure bandwidth, set quality to fit (QUIC mode)
- **Windows IDD driver** — low-overhead capture on Windows

#### From KasmVNC (most innovative open source)
- **Per-rectangle quality tracking** — frequency-based quality map, not whole-frame
- **Multi-encoder mixing** — different encoders for different regions in same frame
- **FFmpeg dynamic loading** — dlopen at runtime, graceful fallback if absent
- **DLP features** — screen redaction, clipboard controls, watermarking (enterprise play)
- **QOI encoding for LAN** — ultra-fast lossless for local networks
- **Video mode auto-detection** — 5s sustained 45% change → switch to video codec
- **TBB parallel encoding** — multi-thread per-rectangle encoding

#### From Sunshine/Moonlight (lowest latency)
- **Zero-copy GPU pipeline** — NVFBC → CUDA → NVENC, never touches CPU RAM
- **AV1 codec support** — 30% better compression than H.264 at same quality
- **Frame pacing** — precise timing to match display refresh rate
- **HDR support** — 10-bit color, tone mapping

#### From RustDesk (best UX)
- **NAT traversal** — UDP hole punching + relay server, works behind firewalls
- **P2P architecture** — no server infrastructure needed for basic use
- **Web client** — browser access option
- **File transfer** — drag and drop files

#### From Parsec (best gaming feel)
- **BUD protocol** — custom reliable UDP, 97% NAT traversal success
- **Client-side prediction** — input prediction for <16ms perceived latency
- **Zero-copy color conversion** — GPU pixel shaders for RGBA→NV12

---

## TODO — Prioritized Roadmap

### Tier 1: Core Performance (biggest impact)

| # | Task | Why | Difficulty | Reference |
|---|------|-----|-----------|-----------|
| T1.1 | **NVENC GPU encoding** | 15ms→2ms encode latency, unlock 4K60 | High | Sunshine's pipeline |
| T1.2 | **QUIC/UDP transport** | Eliminate TCP head-of-line blocking on WAN | High | DCV QUIC, `quinn` crate |
| T1.3 | **VAAPI GPU encoding** | GPU encode on Intel/AMD (no NVIDIA needed) | Medium | KasmVNC FFmpeg integration |
| T1.4 | **x264 software encoder** | 2-3x better compression than OpenH264 (has B-frames, CABAC) | Medium | Replace `openh264` with `x264` via FFmpeg |
| T1.5 | **AV1 encoding** | 30% better than H.264, royalty-free | Medium | SVT-AV1, `rav1e` |

### Tier 2: Essential Features (usability)

| # | Task | Why | Difficulty | Reference |
|---|------|-----|-----------|-----------|
| T2.1 | **Clipboard sync** | Can't copy/paste = unusable for office work | Medium | KasmVNC clipboard DLP model |
| T2.2 | **Audio forwarding** | Video calls, media playback | High | PulseAudio capture → Opus encode → decode on client |
| T2.3 | **Adaptive bitrate** | Auto-adjust quality based on bandwidth, not fixed | Medium | DCV bandwidth-first (QUIC), KasmVNC congestion control |
| T2.4 | **Wayland capture** | X11 is dying, GNOME/KDE default to Wayland now | High | PipeWire + xdg-desktop-portal, `libei` for input |
| T2.5 | **Web client** | Browser access without installing anything | High | WebTransport (QUIC) + WebCodecs (H.264 HW decode) |

### Tier 3: Quality Improvements (refinement)

| # | Task | Why | Difficulty | Reference |
|---|------|-----|-----------|-----------|
| T3.1 | **Per-region quality tracking** | Text areas need higher quality than video areas | Medium | KasmVNC EncodeManager per-rect scoring |
| T3.2 | **Multi-encoder mixing** | Use zstd for text tiles + H.264 for motion regions simultaneously | Medium | KasmVNC multi-encoder |
| T3.3 | **SIMD color conversion** | BGRA→YUV is currently scalar, 4x speedup possible | Low | SSE2/NEON intrinsics, or `wide` crate |
| T3.4 | **Progressive lossless** | Send lossless tiles incrementally (spread bandwidth) instead of burst | Low | Prioritize visible area first |
| T3.5 | **Capture optimization** | Use DMA-BUF/KMS on Linux, reduce memcpy | Medium | Sunshine DMA-BUF capture |

### Tier 4: Production Readiness

| # | Task | Why | Difficulty | Reference |
|---|------|-----|-----------|-----------|
| T4.1 | **Multi-monitor** | Many dev setups have 2+ monitors | Medium | DCV up to 4 displays |
| T4.2 | **USB device redirection** | Hardware tokens, drawing tablets | High | DCV USB remotization |
| T4.3 | **Session management** | Multiple users, access control | Medium | KasmVNC multi-user + permissions |
| T4.4 | **NAT traversal** | Work behind firewalls without port forwarding | High | RustDesk relay model, STUN/TURN |
| T4.5 | **File transfer** | Drag-and-drop files between local and remote | Medium | RustDesk implementation |
| T4.6 | **Bandwidth statistics UI** | Show FPS, latency, bandwidth in client overlay | Low | — |
| T4.7 | **Config file** | YAML/TOML config instead of CLI flags only | Low | KasmVNC YAML model |

### Tier 5: Advanced / Differentiating

| # | Task | Why | Difficulty | Reference |
|---|------|-----|-----------|-----------|
| T5.1 | **Client-side input prediction** | Predict scrolling/dragging locally, correct when server responds | High | Parsec, game netcode |
| T5.2 | **GPU sharing (multi-session)** | Multiple sessions share one GPU without vGPU | Very High | DCV dcv-gl, VirtualGL |
| T5.3 | **DLP features** | Watermark, clipboard control, screen redaction | Medium | KasmVNC DLP |
| T5.4 | **Headless GPU rendering** | EGL headless + GPU encode for cloud workstations | High | DCV virtual sessions |
| T5.5 | **Recording / playback** | Record sessions for audit/training | Medium | — |

---

## Test Plan

### Unit Tests (existing)

Already covered: tile differ, color conversion, crypto, H.264 encode/decode, protocol serialization, zstd compression.

### Integration Tests (to add)

| Test | What It Verifies | How |
|------|-----------------|-----|
| **End-to-end encrypted roundtrip** | Server encrypts → client decrypts correctly | Mock server with --key, client connects, verify frames |
| **Reconnect behavior** | Client reconnects after server restart | Start server, connect client, kill server, restart, verify client reconnects |
| **Lossless quality update** | Static frame triggers lossless update | Send identical frames, verify TileUpdate is sent after delay |
| **Coordinate mapping** | Mouse position maps correctly at different scales | Unit test: map_mouse at various window/server ratios |
| **Wrong key rejection** | Mismatched keys fail gracefully | Server with key A, client with key B, verify error |
| **Large frame handling** | 4K resolution doesn't crash or OOM | Capture/encode/decode 3840x2160 frame |
| **Bandwidth measurement** | Measure actual bytes/sec at various content types | Static desktop, scrolling text, video playback |

### Performance Benchmarks (to add)

| Benchmark | Metric | Target |
|-----------|--------|--------|
| Capture latency | ms per frame | <5ms (scrap), <1ms (DMA-BUF) |
| Encode latency | ms per 1080p frame | <15ms (OpenH264), <3ms (NVENC) |
| Decode latency | ms per frame | <5ms |
| End-to-end latency | input → pixel update | <50ms LAN, <100ms WAN |
| Bandwidth (static desktop) | KB/s | <10 KB/s (skip + lossless sent once) |
| Bandwidth (text scrolling) | KB/s | <500 KB/s (H.264 P-frames) |
| Bandwidth (video playback) | Mbps | <5 Mbps at 1080p30 |
| CPU usage (server, 1080p30) | % | <30% single core (OpenH264), <5% (NVENC) |
| CPU usage (client) | % | <10% |
| Memory (server) | MB | <200 MB |
| Memory (client) | MB | <100 MB |

### Platform Test Matrix

| Platform | Server | Client | Status |
|----------|--------|--------|--------|
| Linux X11 (Ubuntu 22.04) | ✅ primary target | ✅ | Untested |
| Linux Wayland | ❌ not yet | ✅ | — |
| Windows 10/11 | ✅ (DXGI capture) | ✅ | Untested |
| macOS (Apple Silicon) | ❌ dropped | ✅ | Build OK, capture needs permission |
| macOS (Intel) | ❌ dropped | ✅ | Untested |
| Browser (Web client) | — | ❌ not yet | — |

---

## Technical Debt

| Item | Severity | Fix |
|------|----------|-----|
| `has_changes()` sampling can miss small changes | Low | Accept it — full diff runs anyway for lossless |
| BGRA→YUV conversion via `pixel_f32()` (float, per-pixel callback) | Medium | Implement `RGB8Source` for direct slice access, or SIMD |
| `TileDiffer::send_lossless_update` creates/destroys a temp differ | Low | Add a `diff_all()` method that skips comparison |
| Unused `encode_zstd` warning when not in lossless path | Trivial | Already used now for lossless update |
| Integration test for `pipeline_test.rs` has unused `bgra_to_yuv420` import | Trivial | Remove it |
| Mock server doesn't handle input or encryption | Low | Add --key support to mock_server |
| No graceful shutdown (Ctrl+C handling) | Low | Add signal handler |
| Client spawns threads that leak on reconnect | Medium | Use scoped threads or track JoinHandles |

---

## Design Decisions Log

| Decision | Rationale | Alternatives Considered |
|----------|-----------|------------------------|
| Rust | Memory safety, performance, trait-based abstraction | C++ (Sunshine), Go (n.eko) |
| OpenH264 over x264 | Zero system deps (auto-downloads), BSD license | x264 (better compression but GPL + system dep) |
| ChaCha20 over TLS | No cert management, works with TCP split, simpler | rustls (cert complexity), native-tls (split problem) |
| Two TCP connections avoided | Kept single connection + split via try_clone | Two connections (cleaner but more protocol complexity) |
| minifb over winit+wgpu | Simplest possible display (3 API calls) | winit+pixels (more features), egui (UI) |
| Tile-based lossless (not full-frame PNG) | Can send partial updates, parallelizable | Full-frame zstd (simpler but larger burst) |
| 64x64 tile size | Balance between detection granularity and overhead | 32x32 (finer but more tiles), 128x128 (coarser) |
| Counter nonce (not random) | Prevents replay, detects out-of-order/corruption | Random nonce (no replay protection) |

---

## Usage

```bash
# Build
cargo build --release

# Server (generates encryption key)
cargo run --release -p phantom-server
# prints: --key <64 hex chars>

# Server (custom settings)
cargo run --release -p phantom-server -- \
  --listen 0.0.0.0:9900 \
  --fps 30 \
  --bitrate 5000 \
  --quality-delay-ms 500 \
  --key <hex>

# Client
cargo run --release -p phantom-client -- \
  --connect <server-ip>:9900 \
  --key <hex>

# Testing without encryption
cargo run --release -p phantom-server -- --no-encrypt
cargo run --release -p phantom-client -- --connect 127.0.0.1:9900 --no-encrypt

# Mock server (no screen capture needed, for testing client)
cargo run --release --bin mock_server

# Run tests
cargo test
```
