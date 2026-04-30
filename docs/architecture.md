# Phantom — Architecture

A high-performance, open-source remote desktop built in Rust. Low latency,
single-binary deployment, browser + native access.

~18,000 lines Rust (across 6 crates), 136 tests, MIT license. Runs on Linux +
Windows; native client also runs on macOS.

## Design decisions

### No GStreamer

Direct function calls (capture → encode → send) = 0ms pipeline overhead.
Sunshine and Parsec don't use GStreamer either. Our pipeline is 3 steps,
not 20 elements with buffer copies.

### WebRTC browser path (current)

The browser WebRTC path now uses **video/audio media tracks + input/control
DataChannels**. This replaced the old "all DataChannel" design for desktop
video.

Current shape:

- WebRTC is the default browser transport.
- WSS remains available with `?wss` / `?ws` for debugging or UDP-blocked environments.
- Signaling stays on `POST /rtc` (same HTTPS endpoint family as WSS).
- Backend is in-tree (`transport/webrtc/backend_phantom.rs`) using
  `dimpl` (DTLS), `phantom-sctp` (SCTP/DataChannel), and `stun-types`
  (ICE/STUN messages).

### WebRTC, not WebTransport

WebTransport requires HTTPS + certificates. Self-signed ≤14 days in Chrome.
Pure IP (most users) doesn't work. WebRTC works with any IP, has built-in
DTLS + NAT traversal.

### In-tree WebRTC backend

The WebRTC transport loop and protocol wiring are Phantom-owned:

- one long-lived UDP socket + one run loop thread
- ICE/STUN handling + DTLS handshake
- SRTP/SRTCP packetization for media tracks
- SCTP DataChannel bridge for input/control messages

### Dual web transport: WebRTC default + WSS fallback

- **WebRTC** (default browser path, feature `webrtc`): browser media tracks
  for video/audio + DataChannels for input/control. POST `/rtc` signaling.
- **WSS** (`?wss` / `?ws`): WebSocket upgrade on the same HTTPS port 9900.
  Reliable fallback for debugging or UDP-blocked environments.
- **Native**: raw QUIC (no browser overhead) + raw TCP.
- All produce same `Box<dyn MessageSender/Receiver>` → same session loop.

## Session architecture

### Pipeline trait (0.4.7 refactor)

Three capture+encode backends — CPU, NVFBC→NVENC, DXGI→NVENC — share one
session loop via a single trait. See `crates/server/src/pipeline.rs`.

```rust
pub trait Pipeline {
    fn tick(&mut self, ctx: TickCtx) -> Result<Option<TickResult>>;
    fn dimensions(&self) -> (u32, u32);
    fn bitrate_kbps(&self) -> u32;
    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> { /* default: unsupported */ }
    fn congestion_mut(&mut self) -> Option<&mut CongestionTracker> { None }
    fn log_label(&self) -> &'static str { "stats" }
    fn prepare(&mut self) -> Result<()> { Ok(()) }
}
```

Each tick the session loop gives the pipeline `had_input` (for GPU paths
that sleep briefly to let the screen update) and `needs_keyframe` (for the
periodic 2-second keyframe). Pipeline returns `Some(encoded_frame)` or
`None` (capture empty / differ saw no change / congestion asked to skip).

Impls:
- `CpuPipeline` — scrap capture → OpenH264 / NVENC encode, `TileDiffer`
  gates encode when the screen is unchanged, `CongestionTracker` skips
  frames under network pressure.
- `NvfbcNvencPipeline` — NVFBC grab → NVENC encode, CUDA device pointer
  never reaches CPU memory.
- `DxgiNvencPipelineAdapter` — thin wrapper around the fused struct in
  `crates/gpu/src/dxgi_nvenc.rs`.

The session loop (`run_session` in `crates/server/src/session.rs`) does
input pumping, clipboard, audio drain, keepalive, stats, adaptive
bitrate, frame pacing, file-transfer drain — all transport- and
backend-agnostic.

### Periodic keyframes (2s interval)

Server forces IDR every 2 seconds. Recovers from:
- RTP loss / jitter spikes
- Client decoder errors
- Browser tab backgrounding/foregrounding
- Client reconnect / resume

### Session resume + replacement

- **Hello**: server sends resolution, codec, audio flag, opaque
  `session_token` (32 random bytes).
- **Resume**: client reconnects with `(session_token, last_sequence)`;
  server replies `ResumeOk` + forces keyframe, else a fresh `Hello`.
- **Replacement**: new client with a different `client_id` takes over
  an active session; old client is sent `Disconnect` and bailed out.
- **Ghost-set**: a bounded LRU of recently kicked `client_id`s rejects
  auto-reconnect from the dead client so a forgotten browser tab can't
  thrash a real user's session. See `crates/server/src/doorbell.rs`.

### Session reconnect bugs to not reintroduce

1. **`recv_msg()` infinite spin**: MUST detect
   `mpsc::TryRecvError::Disconnected` and return error. Otherwise the
   receive loop thread spins forever, the session never ends, and no
   reconnect happens.
2. **Session publish ordering**: only publish the session bridge after
   both `input` and `control` DataChannels are opened, otherwise early
   control traffic can race a partially ready bridge.
3. **UDP socket lifecycle**: do NOT create one socket per session. One
   socket for the whole server. WebRTC run_loop manages it.
4. **`force_keyframe` at session start**: new client needs an IDR frame.
   Pipeline impls force one in `prepare()` or on the first encode.
5. **Web client `got_keyframe` guard**: WebCodecs throws if first frame
   is delta. Client skips all delta frames until first IDR arrives.

## WebRTC run_loop

- One UDP socket for the entire server lifetime (never rebind).
- One `run_loop` thread managing one active client at a time.
- 1ms UDP socket timeout for responsive polling (was 50ms — caused
  visible lag).
- `poll_output` after `drain_outgoing` — flush written data immediately.
- New POST /rtc → drain all pending `Rtc`, keep latest → replace active
  client immediately.
- Session delivered via `Mutex<Option>` slot (always latest, stale
  auto-dropped).
- Bounded queues in the bridge (`sync_channel(8)` video,
  `sync_channel(64)` audio/control); media path uses `try_send`
  (backpressure, no blocking).
- Video/audio are sent over RTP media tracks; DataChannels are used for
  input/control only.

## GPU pipeline

All NVIDIA libraries loaded at runtime via dlopen — compiles on any
machine, GPU optional.

### Flows

**NVENC with CPU capture** (scrap → NVENC):
```
Frame.data (BGRA CPU) → bgra_to_nv12 (CPU, AVX2 SIMD) → cuMemcpyHtoD →
NVENC encode (GPU) → H.264 bytes
~8ms at 1080p
```

**NVFBC→NVENC zero-copy** (all GPU, Linux):
```
NVFBC grab → CUdeviceptr (NV12, GPU) → NVENC encode (GPU) → H.264 bytes
~4ms at 1080p (capture 0.4ms + encode 3.5ms)
```

**DXGI→NVENC zero-copy** (all GPU, Windows):
```
Desktop Duplication → ID3D11Texture2D → NVENC encode → H.264 bytes
~4-8ms at 1080p
```

### CUDA context management (hard-won)

- Use `cuDevicePrimaryCtxRetain` — NVFBC internally uses the primary
  context. `cuCtxCreate` creates a separate context that conflicts.
- NVFBC holds a context lock. Must call `NvFBCReleaseContext` before
  NVENC operations, `NvFBCBindContext` before NVFBC grab.
- NVENC's `encode_registered()` checks `ctx_get_current()` and only does
  `ctx_push` if needed (avoids double-push deadlock).

### NVFBC struct sizes (critical)

- NVFBC embeds `sizeof` in the version field. Wrong size = buffer
  overflow = silent memory corruption.
- Verified sizes from nvfbc-sys bindgen: `CreateHandleParams=40`,
  `CaptureSessionParams=64`, `GrabFrameParams=32`, `FrameGrabInfo=48`.
- Use opaque byte arrays (not Rust structs with named fields) to
  guarantee correct sizes.

### NVFBC function loading

- Do NOT use `NvFBCCreateInstance` — it has strict API version checks
  that vary by driver.
- Instead, dlsym each function directly: `NvFBCCreateHandle`,
  `NvFBCToCudaGrabFrame`, etc.

## Windows Service mode

Architecture follows RustDesk/Sunshine pattern:

- **Service** (Session 0, LocalSystem): manages lifecycle, accepts
  client connections, forwards encoded frames.
- **Agent** (user session): launched via `CreateProcessAsUser` with
  SYSTEM token (not user token — required for Winlogon desktop access
  on the lock screen).
- **IPC**: two named pipes — `PhantomIPC_up_{session_id}` for frames,
  `PhantomIPC_down_{session_id}` for input. Windows synchronous I/O
  deadlocks if you use a single duplex pipe with concurrent read+write
  on the same handle.
- Agent does DXGI→NVENC encoding and sends H.264 bytes over pipe
  (~50KB, not 8MB raw frames).
- Service uses `run_session_ipc()` which reuses `SessionRunner` for
  input/clipboard/keepalive/audio/stats.
- On lock screen: DXGI fails → agent falls back to GDI capture +
  OpenH264 → auto-recovers to DXGI on unlock.
- Agent calls `OpenInputDesktop()` + `SetThreadDesktop()` before
  capture (follows the active desktop like RustDesk/Sunshine).
- `--install` / `--uninstall` / `--install-vdd` manage the service +
  Virtual Display Driver.

## Linux VM autologin mode

`install.sh server --autologin` (for dedicated remote-access Linux VMs):

- GDM `AutomaticLogin` + `TimedLogin` (5s) — session auto-restores after
  sign-out. GDM 42 (Ubuntu 22) has a known `TimedLogin` regression; a
  systemd timer watchdog polls every 30s and kicks `gdm3` if no seat0
  session for the target user.
- dconf overrides disable screen lock, idle timeout, and the "Switch
  User" menu entry (Switch User backgrounds the user's X session on a
  different VT while phantom is pinned to `DISPLAY=:0` → phantom
  captures a black screen).
- Clears + re-seeds the keyring; an autostart hook unlocks with empty
  password so Chrome/Evolution don't pop a keyring dialog.
- Drops an XDG autostart `.desktop` for phantom-server (with a wrapper
  that kills any stale instance first, since phantom-server can survive
  gnome-session exit via PPID-1 re-parenting and hold ports 9900/9901).

## Transport abstraction

`run_session` takes `Box<dyn MessageSender>` + `Box<dyn MessageReceiver>`.
All transports (TCP, QUIC, WebSocket, WebRTC) implement the same traits.
Adding a new transport = new file under `crates/server/src/transport/` +
implement the two traits + one-line init change.
