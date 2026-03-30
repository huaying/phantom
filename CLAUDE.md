# Phantom — Developer Guide for Claude

## What This Project Is

Phantom is a remote desktop built in Rust. Native client + web client (WASM). The key differentiators are two-phase lossless rendering and WebRTC DataChannel for the web client.

## Build Commands

```bash
cargo build --release                                    # native
wasm-pack build crates/web --target web --no-typescript  # WASM (must run before server build)
cargo build --release -p phantom-server                  # server embeds WASM via include_bytes!
cargo test                                               # 21 tests
cargo clippy --release                                   # must be zero warnings
```

## Test Environment

```bash
# Docker with XFCE desktop
docker build -t phantom .
docker run --rm -p 9900:9900 -p 9901:9901 -p 9902:9902/udp -e PHANTOM_HOST=127.0.0.1 phantom server-web

# Native client
cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900

# Web client: open http://127.0.0.1:9900 in Chrome
```

## Architecture Decisions (why we chose what we chose)

### No GStreamer
Direct function calls (capture → encode → send) = 0ms pipeline overhead. Sunshine and Parsec don't use GStreamer either. Our pipeline is 3 steps, not 20 elements.

### WebRTC DataChannel, not Media Track
Media Track adds 30-80ms jitter buffer (designed for video calls). DataChannel delivers raw bytes → WebCodecs GPU decode → Canvas. Measured: 20-50ms vs 80-150ms.

### WebRTC, not WebTransport
WebTransport requires HTTPS + certificates. Pure IP doesn't work (most users don't have domains). WebRTC works with any IP, has built-in DTLS encryption and NAT traversal.

### HTTP POST signaling (str0m pattern)
No WebSocket needed for SDP exchange. Browser creates offer → POST /rtc → server returns answer. Single HTTP request-response. This avoids the chicken-and-egg problem where WebSocket signaling requires the session to be running first.

### str0m (sans-IO WebRTC)
Pure Rust, lightweight (~15K lines), no tokio dependency for WebRTC path. We provide the UDP socket, str0m provides the logic. Official `chat.rs` example shows the exact pattern we use.

### Smart encoding (dirty% threshold)
CPU-only hosts: 90% of updates are small (typing, cursor). Encoding only dirty tiles with zstd costs 0.1ms vs 15ms for full H.264. Server hides remote cursor so mouse movement = 0 dirty tiles = 0 CPU.

### Rust WASM for web client
Shares phantom-core code (protocol, input, clipboard). One language, one codebase. bincode works in WASM — no custom serialization needed.

## Key Implementation Details

### WebRTC run_loop (str0m official pattern)
- **One UDP socket** for entire server lifetime (never rebind)
- **One `run_loop` thread** managing one active client
- New POST /rtc → replace active client immediately (browser refreshed)
- `rtc.is_alive()` + channel disconnect detection for cleanup
- Session delivered via `Mutex<Option>` (always latest, stale auto-dropped)

### Session reconnect (hard-won knowledge)
- `recv_msg()` MUST detect `mpsc::TryRecvError::Disconnected` and return error, otherwise receive_loop thread spins forever and session never ends
- Keepalive ping every 1s via `sender.send_msg(Ping)` — detects dead channel when run_loop replaces client
- Hello message MUST go through video DC (same channel as VideoFrame) — otherwise Hello arrives after keyframe via slower control DC, decoder not configured, "Key frame is required" error
- `force_keyframe()` at start of every `run_session` — new client needs IDR frame

### Smart encoding flow
```
capture → TileDiffer (64x64 blocks) → dirty count
  if dirty < 10% → zstd tiles only (TileUpdate message)
  if dirty ≥ 10% → H.264 full frame (VideoFrame message)
  if static 2s   → zstd lossless all tiles (pixel-perfect refinement)
```

### Transport abstraction
`run_session()` takes `Box<dyn MessageSender>` + `Box<dyn MessageReceiver>`. Doesn't care if it's TCP, QUIC, WebSocket, or WebRTC. All 4 transports implement the same traits.

## Common Pitfalls

- **WASM build order matters**: must `wasm-pack build` BEFORE `cargo build -p phantom-server` because server embeds WASM via `include_bytes!`
- **Docker UDP port**: WebRTC needs `-p 9902:9902/udp` AND `-e PHANTOM_HOST=127.0.0.1` for ICE candidate
- **str0m Receive.destination**: must match candidate_addr (not socket bind addr 0.0.0.0)
- **macOS Cmd key**: don't send Meta/Super to server — gets stuck after Cmd+Tab, causes Super+E/R/P shortcuts in XFCE
- **Unbounded channels**: video channel uses `sync_channel(30)` with `try_send` — drops frames on backpressure instead of blocking
- **Mutex poison**: use `unwrap_or_else(|e| e.into_inner())` not `.unwrap()`

## What's Next (from DESIGN.md roadmap)

Priority order:
1. **Audio forwarding** — biggest feature gap
2. **WAN testing** — need a real cloud VM to verify latency
3. **WS/WebRTC adaptive fallback** — auto-detect best transport
4. **GPU encoding (NVENC/VAAPI)** — need GPU machine (company computer has this!)
5. **NVFBC capture** — zero-copy GPU capture on NVIDIA

## File Map

```
crates/core/src/
  capture.rs      FrameCapture trait
  encode.rs       FrameEncoder + Encoder (tile) + FrameDecoder traits
  transport.rs    MessageSender/Receiver traits
  protocol.rs     Message enum (Hello, VideoFrame, TileUpdate, Input, Clipboard, Paste, Ping)
  tile.rs         TileDiffer (64x64 dirty detection)
  crypto.rs       ChaCha20-Poly1305 encryption (feature-gated, optional for WASM)

crates/server/src/
  main.rs              Session loop, transport selection, smart encoding, keepalive
  capture_scrap.rs     ScrapCapture (impl FrameCapture)
  encode_h264.rs       OpenH264Encoder (impl FrameEncoder)
  encode_zstd.rs       ZstdEncoder (impl Encoder for tiles)
  input_injector.rs    enigo input injection
  transport_tcp.rs     TCP Plain/Encrypted sender/receiver
  transport_quic.rs    QUIC via quinn
  transport_ws.rs      WebServerTransport (HTTP static + WS fallback + WebRTC orchestration)
  transport_webrtc.rs  str0m run_loop, ActiveClient, WebRtcSender/Receiver

crates/client/src/
  main.rs              winit event loop, reconnect, WebRTC/QUIC/TCP
  display_winit.rs     softbuffer rendering, coordinate mapping, cursor overlay
  input_capture.rs     winit → phantom KeyCode mapping

crates/web/src/
  lib.rs               WASM entry, WebRTC setup, WebCodecs decode, Canvas render, input
```
