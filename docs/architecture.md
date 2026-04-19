# Phantom — Architecture

A high-performance, open-source remote desktop built in Rust. Target:
Parsec-class latency (~20-50ms), single binary deployment, browser +
native access.

~18,000 lines Rust (across 6 crates), MIT license. Runs on Linux +
Windows; native client also runs on macOS.

## Competitive Position

```
                    Latency     Text Quality    Deploy        Web Client  Open Source
Parsec              15-30ms     lossy(blurry)   simple        ❌          ❌
NICE DCV            30-60ms     pixel-perfect   medium        ✅(limited) ❌
KasmVNC             80-150ms    pixel-perfect   Docker        ✅          ✅
Neko                80-150ms    lossy(blurry)   Docker        ✅          ✅
Selkies (Google)    70-120ms    lossy(blurry)   complex       ✅          ✅
RustDesk            50-100ms    lossy(blurry)   simple        ✅(beta)    ✅
Phantom             20-50ms     lossy(H.264)    single binary ✅          ✅
```

### Five unique advantages
1. **DataChannel + WebCodecs** — same approach as Parsec/Zoom, bypasses
   jitter buffer (30-80ms saved)
2. **WSS fallback on same port** — WebSocket + WebCodecs as reliable
   fallback (validated by Helix at scale)
3. **Single binary** — web client embedded, no Docker/GStreamer/coturn
4. **Rust + WASM code sharing** — one codebase for server + web client
5. **~17K lines** — vs KasmVNC 200K+, Neko 15K+, RustDesk 150K

### Lessons from competitors
- **Parsec** (BorgGames/streaming-client): DataChannel reliable+ordered for
  video, MSE decode. Only other production DataChannel video impl.
- **Zoom**: unreliable DataChannel for video, WASM decode. Validated at
  massive scale.
- **Neko/Selkies**: WebRTC Media Track (RTP), not DataChannel. Browser
  handles decode. Simpler but 30-80ms jitter buffer.
- **Helix**: killed WebRTC entirely, WebSocket + WebCodecs. Reports
  20-30ms lower latency than their WebRTC setup.
- **Sunshine**: no web client. Custom UDP + RTP + Reed-Solomon FEC for
  native Moonlight clients.
- **NICE DCV**: QUIC transport (we have it), GPU sharing (future)
- **KasmVNC**: per-rectangle quality tracking, multi-encoder mixing, DLP
- **RustDesk**: NAT traversal, P2P, file transfer

## Architecture decisions

### No GStreamer
Direct function calls (capture → encode → send) = 0ms pipeline overhead.
Sunshine and Parsec don't use GStreamer either. Our pipeline is 3 steps,
not 20 elements with buffer copies.

### WebRTC DataChannel, not Media Track
Media Track adds 30-80ms jitter buffer (designed for video calls).
DataChannel delivers raw bytes instantly → WebCodecs GPU decode → Canvas.
Measured: 20-50ms vs 80-150ms.

### WebRTC, not WebTransport
WebTransport requires HTTPS + certificates. Self-signed ≤14 days in Chrome.
Pure IP (most users) doesn't work. WebRTC works with any IP, has built-in
DTLS + NAT traversal.

### HTTP POST signaling (str0m pattern)
Browser creates offer → POST /rtc → server returns answer. Single HTTP
round-trip. No WebSocket signaling needed. Avoids chicken-and-egg (session
must run before signaling can work).

### str0m (sans-IO WebRTC)
Pure Rust, ~15K lines, no tokio for WebRTC path. We provide the UDP
socket; str0m provides the protocol logic. Official `chat.rs` pattern: one
socket, one run_loop, demux via `rtc.accepts()`.

**CRITICAL: str0m SCTP cannot deliver messages >16KB reliably.**
Regardless of reliable/ordered settings, large DataChannel messages
(e.g. 70KB H.264 keyframe) silently fail. Root cause: str0m's `ch.write()`
returns `Ok(false)` when the 128KB cross-stream SCTP buffer is full, and
phantom was ignoring this return value. Fix: backpressure via
`set_buffered_amount_low_threshold()` + `Event::ChannelBufferedAmountLow`
to pause/resume writes, with per-channel pending queues.

### Always H.264 full frames (tile mode disabled)
Tile-based rendering (zstd per-tile) was disabled — caused visual tearing
when mixed with H.264 over high latency. Now every frame change triggers a
full H.264 encode. TileDiffer still detects change to skip encode on
static frames.

### Periodic keyframes (2s interval)
Server forces IDR keyframe every 2 seconds. Recovers from:
- WebRTC DataChannel packet loss (unreliable mode future)
- Client decoder errors
- Browser tab backgrounding/foregrounding

### Dual web transport: WSS default + WebRTC optional
- **WSS** (default): WebSocket upgrade on same HTTPS port 9900. No message
  size limits. Reliable. Validated by Helix as production-viable.
- **WebRTC DataChannel** (`--features webrtc` build flag, `?rtc` URL
  param): POST /rtc signaling, str0m 0.18, reliable+ordered. Needs
  chunking for messages >16KB (SCTP limitation). Only needed for future
  NAT traversal.
- **Native**: raw QUIC (no browser overhead, 15-30ms target)
- All produce same `Box<dyn MessageSender/Receiver>` → same session loop

## Key implementation details

### WebRTC run_loop (str0m official pattern)
- One UDP socket for the entire server lifetime (never rebind — this was
  a hard bug)
- One `run_loop` thread managing one active client at a time
- 1ms UDP socket timeout for responsive polling (was 50ms — caused
  visible lag)
- `poll_output` after `drain_outgoing` — flush written data immediately
- New POST /rtc → drain all pending Rtc, keep latest → replace active
  client immediately
- Session delivered via `Mutex<Option>` slot (always latest, stale auto-dropped)
- Bounded `sync_channel(30)` for video with `try_send` (backpressure, no
  blocking)
- Chunking: messages >16KB split into chunks before `ch.write()`. Client
  reassembles.

### Session reconnect (hard-won bugs)
These bugs took significant debugging. Don't reintroduce them.

1. **`recv_msg()` infinite spin**: MUST detect
   `mpsc::TryRecvError::Disconnected` and return error. Otherwise the
   receive loop thread spins forever, the session never ends, and no
   reconnect happens.
2. **Hello ordering**: Hello MUST go through video DC (same as
   `VideoFrame`). Control DC may deliver slower → decoder not configured
   when keyframe arrives → "Key frame is required" error.
3. **UDP socket lifecycle**: Do NOT create one socket per session. One
   socket for the whole server. str0m run_loop manages it. Old approach
   (bind/rebind per session) caused port conflicts.
4. **`force_keyframe` at session start**: New client needs IDR frame.
   Call `video_encoder.force_keyframe()` + `differ.reset()` at the top of
   `run_session()`.
5. **Web client `got_keyframe` guard**: WebCodecs throws if first frame is
   delta. Client skips all delta frames until first IDR arrives. Handles
   the race condition where P-frames arrive before keyframe.

### Session affinity (ghost-set)
Each client (native or web) generates a 16-byte `client_id` once per
process / tab lifetime. The doorbell tracks the current owner and a
bounded LRU set of recently kicked ids. Auto-reconnect attempts from a
ghost id are rejected so a forgotten browser tab can't thrash a real
user's session. See `crates/server/src/doorbell.rs`.

### Encoding flow
```
capture → TileDiffer (64x64 blocks) → any dirty?
  if dirty → H.264 full frame (VideoFrame)
  if static → skip encode (zero CPU)
  every 2s → force keyframe (IDR)
```

TileDiffer detects change. Hidden remote cursor means mouse movement alone
yields 0 dirty tiles → 0 CPU.

### Windows service mode (Session 0 + Agent)
Architecture follows RustDesk/Sunshine pattern:
- **Service** (Session 0, LocalSystem): manages lifecycle, accepts client
  connections, forwards encoded frames.
- **Agent** (user session): launched via `CreateProcessAsUser` with
  SYSTEM token (not user token — required for Winlogon desktop access on
  lock screen).
- **IPC**: two named pipes (`PhantomIPC_up` for frames,
  `PhantomIPC_down` for input) — Windows synchronous I/O deadlocks if
  you use a single DUPLEX pipe with concurrent read+write on the same
  handle.
- Agent does DXGI→NVENC encoding and sends H.264 bytes over pipe (~50KB,
  not 8MB raw frames).
- Service uses `run_session_ipc()` which reuses `SessionRunner` for
  input/clipboard/keepalive/audio/stats.
- On lock screen: DXGI fails → agent falls back to GDI capture +
  OpenH264 → auto-recovers to DXGI on unlock.
- Agent calls `OpenInputDesktop()` + `SetThreadDesktop()` before capture
  (follows the active desktop like RustDesk/Sunshine).
- `--install` / `--uninstall` for service management;
  `--agent-mode` for agent process.

### Transport abstraction
`run_session()` takes `Box<dyn MessageSender>` + `Box<dyn MessageReceiver>`.
All transports (TCP, QUIC, WebSocket, WebRTC) implement the same traits.
Adding a new transport = new file under `crates/server/src/transport/` +
implement traits + one-line init change.

### GPU pipeline (`crates/gpu/`)
All NVIDIA libraries loaded at runtime via dlopen — compiles on any
machine, GPU optional.

**NVENC encoder flow** (CPU input path):
```
Frame.data (BGRA CPU) → bgra_to_nv12 (CPU, AVX2 SIMD) → cuMemcpyHtoD →
NVENC encode (GPU) → H.264 bytes
~8ms at 1080p (was ~10ms before SIMD)
```

**NVFBC→NVENC zero-copy flow** (all GPU):
```
NVFBC grab → CUdeviceptr (NV12, GPU) → NVENC encode (GPU) → H.264 bytes
~4ms at 1080p (capture 0.4ms + encode 3.5ms)
```

**CUDA context management** (hard-won lessons):
- Use `cuDevicePrimaryCtxRetain` — NVFBC internally uses the primary
  context. `cuCtxCreate` creates a separate context that conflicts.
- NVFBC holds a context lock. Must call `NvFBCReleaseContext` before
  NVENC operations, `NvFBCBindContext` before NVFBC grab.
- NVENC's `encode_registered()` checks `ctx_get_current()` and only does
  `ctx_push` if needed (avoids double-push deadlock).

**NVFBC struct sizes** (critical):
- NVFBC embeds `sizeof` in the version field. Wrong size = buffer
  overflow = silent memory corruption.
- Verified sizes from nvfbc-sys bindgen: `CreateHandleParams=40`,
  `CaptureSessionParams=64`, `GrabFrameParams=32`, `FrameGrabInfo=48`.
- Use opaque byte arrays (not Rust structs with named fields) to
  guarantee correct sizes.

**NVFBC function loading**:
- Do NOT use `NvFBCCreateInstance` — it has strict API version checks
  that vary by driver.
- Instead, dlsym each function directly: `NvFBCCreateHandle`,
  `NvFBCToCudaGrabFrame`, etc.

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
DXGI→NVENC (zero-copy):     30-47 fps (capped by 52Hz refresh rate)
```
