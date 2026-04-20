# Phantom — Common Pitfalls

These are bugs we've actually shipped and had to track down. If a change
you're making touches one of these areas, re-read the relevant entry first.

## Build / packaging
- **WASM build order**: must `wasm-pack build` BEFORE
  `cargo build -p phantom-server`. Server embeds the WASM via
  `include_bytes!`, so stale WASM = stale browser bundle.
- **WASM feature flag**: `--no-default-features` builds the server
  without WASM (for GPU-only VMs without wasm-pack). Browser will
  receive a stub JS that prints `console.error` and the canvas stays
  blank. Don't use this flag unless you know you don't need the web
  client.
- **Docker WebRTC**: needs `-p 9902:9902/udp` AND
  `-e PHANTOM_HOST=127.0.0.1`.

## Networking
- **str0m DataChannel >16KB**: SCTP silently drops large messages. MUST
  chunk into ≤16KB pieces. Chrome's limit is 256KB but str0m fails well
  below that.
- **str0m `Receive.destination`**: must match `candidate_addr`
  (`127.0.0.1:9902`), not socket bind addr (`0.0.0.0:9902`).
- **WebRTC session zombie**: after ICE disconnect, `send_msg()` swallows
  Full errors. Session never ends. Must detect and terminate.
- **WSS same port**: WS upgrade lives on HTTPS port 9900 (not separate
  port). Avoids self-signed cert rejection for a second port.
- **HTTP query string**: strip `?ws` from path before routing — `/?ws`
  returns 404 otherwise.
- **WS disconnect under high bandwidth**: TLS write can exceed read
  timeout → tungstenite interprets as error. Increased timeout from 5ms
  to 50ms.
- **WS send queue bounded via `sync_channel(30)` + `try_send`**: earlier
  it was unbounded, which made a stalled client (laptop sleep) grow the
  server-side mpsc forever; on wake the client fast-forwarded through
  the backlog. Fixed in 0.4.3; mirrors the WebRTC pattern.
- **HTTPS required for WebCodecs**: non-localhost HTTP is not a secure
  context. Server uses self-signed TLS (rcgen) for HTTPS.
- **QUIC ALPN mismatch**: server sets `alpn_protocols = ["phantom"]` but
  client must also set it. Without matching ALPN, TLS handshake fails
  with "peer doesn't support any known protocol". Fixed in e4487ec.

## Capture / encode (GPU)
- **NVFBC struct sizes**: must match driver's expected sizeof exactly.
  Use opaque byte arrays, not Rust structs.
- **NVFBC `FORCE_REFRESH`**: blocks on driver 550. Use NOWAIT + ensure
  screen activity for new frames.
- **NVFBC needs `DISPLAY`**: set `DISPLAY=:0` (or whichever the X server
  is on) when running on a remote machine. NVFBC captures the X11
  framebuffer.
- **NVFBC + NVENC CUDA context**: use the primary context
  (`cuDevicePrimaryCtxRetain`), not `cuCtxCreate`. Bind/release around
  NVFBC↔NVENC transitions.
- **NVENC GUID by value**: `nvEncGetEncodePresetConfigEx` passes GUIDs by
  value, not by pointer (C ABI quirk).
- **NVENC profile**: must use Baseline. OpenH264 decoder doesn't support
  the High profile NVENC defaults to.
- **NVENC `FORCEIDR`**: value is `2` (`0x2`), not `4`. Wrong value =
  keyframe never sent = client black screen.
- **NVENC SPS/PPS**: NVENC only outputs SPS/PPS on first encode after
  `nvEncInitializeEncoder()`. `force_keyframe()` produces an IDR without
  SPS/PPS. Fix: server saves SPS/PPS from first keyframe and prepends to
  subsequent keyframes that lack it. Do NOT recreate the encoder per
  session (causes CUDA context conflicts on Linux).
- **NVENC `set_repeat_sps_pps` offset**: offset 152 in `NvEncConfig` is
  unreliable across drivers. Driver 537 (L40) ignores it; driver 550
  (A40) returns `INVALID_PARAM`. Use SPS/PPS save+prepend instead.
- **NVENC WebCodecs codec string**: must use `avc1.42c028` (Baseline
  Level 4.0). NVENC outputs Level 4.0 for 1080p. Previous `avc1.42001f`
  (Level 3.1) silently rejected 1080p (exceeds level max 720p).
- **`DxgiNvencPipeline` SPS/PPS**: `set_repeat_sps_pps(true)` is
  unreliable across drivers. SPS/PPS save+prepend now built into
  `DxgiNvencPipeline::capture_and_encode()` itself (shared by console
  mode and agent mode).

## Capture / encode (CPU + DXGI)
- **DXGI `AcquireNextFrame` timeout**: must use a blocking timeout
  (e.g. 33ms), NOT 0. With timeout=0, the capture loop misses frames
  between polls → 15fps instead of 30+fps.
- **DXGI refresh rate**: capture FPS capped by monitor refresh rate
  (DWM). RDP / headless may have a low refresh (15-30Hz). Check with
  `wmic path Win32_VideoController get CurrentRefreshRate`.
- **DXGI on lock screen**: `DXGI_ERROR_KEYED_MUTEX_ABANDONED` (0x887A0026)
  on desktop switch. Agent must drop pipeline, switch desktop, reinit.
  Some drivers (L40 / virtual) lose DXGI entirely until reboot — GDI
  fallback essential.
- **DXGI recreate must drop old duplication first**:
  `IDXGIOutputDuplication` only one per output. Must set
  `self.duplication = None` before `DuplicateOutput()`. Using
  `mem::zeroed()` creates a null COM pointer → crash on Drop.
- **DXGI recreate must reuse same adapter+output**: re-enumerating
  adapters in `recreate()` picks the wrong output (different from
  initial). Store adapter+output_idx and reuse.
- **`force_keyframe` must NOT reset DXGI on periodic timer**:
  `capture.reset()` every 2s causes a pipeline crash loop. Only reset on
  IPC keyframe request (new session). Periodic timer just sets `force_idr`
  flag.
- **OpenH264 SIMD**: must use `phantom_core::color::bgra_to_yuv420`
  (AVX2 SIMD), NOT `pixel_f32()` callback. Per-pixel f32 = ~300ms/frame
  at 1080p; SIMD = ~10ms.

## Decode (client)
- **Client `VideoFrame` decode**: must decode ALL frames sequentially,
  not just the last one. Keyframes get overwritten by empty P-frames in
  the channel buffer when the encoder is fast (GPU).
- **Tile-based rendering (zstd)**: caused visual tearing when mixed with
  H.264 over high latency, and only ever ran in CPU capture mode (never
  on the GPU zero-copy paths). Whole tile path + `TileUpdate` protocol
  message deleted in 0.4.4. Protocol version bumped to 6 so clients
  that still expect `TileUpdate` fail fast at handshake instead of
  silently desyncing. Current protocol is v7 (added `RequestKeyframe`
  in 0.4.8 for the tab-visibility recovery path below; MIN stayed at 6
  since old clients simply don't send that message).
- **Tab-focus fast-forward** (0.4.10 fix, two earlier attempts that
  didn't work are in the git log for reference): when a browser tab is
  backgrounded, the kernel TCP receive buffer accumulates encoded
  video past phantom's bounded server-side mpsc. On focus the browser
  drains + decodes the burst at wire speed → video appears to
  fast-forward through a stale backlog. Neither sequence-based nor
  keyframe-based filtering is reliable because `visibilitychange` and
  the buffered `onmessage` events interleave differently per browser.
  Fix: on `visibilitychange → visible`, web client hard-drops every
  frame for 500ms (covers the burst-dispatch window) then waits for
  the next keyframe before resuming decode. Sends `RequestKeyframe`
  at the same time so the server emits a fresh IDR instead of the
  client having to wait the natural 2s periodic interval.
- **Chrome hardware WebCodecs black screen**: hardware `VideoDecoder`
  defers output callback when the tab isn't fully focused (after URL
  navigation). Fix: use `prefer-software` for decode (~2-4ms vs ~0.5ms
  at 1080p, negligible vs network RTT).
- **Canvas focus required for keyboard**: without `tabindex="0"` +
  `canvas.focus()`, first keypresses go to the browser address bar.
  Auto-focus the canvas on page load.

## Session lifecycle
- **Keepalive**: 1s ping via `sender.send_msg(Ping)` detects dead
  channels after browser refresh.
- **Mutex poison**: use `unwrap_or_else(|e| e.into_inner())` not
  `.unwrap()`.
- **Bounded channels**: WebRTC + WSS video both use `sync_channel(30)` +
  `try_send` — drops on full, never blocks.
- **IPC encoded frames must be sequential**: H.264 P-frames depend on
  previous frames. Never drain-to-latest — forward ALL queued frames in
  order.
- **Keyframe request must come BEFORE wait-for-frame loop**:
  `create_service_session` waits 2s for first frame. On a static desktop
  no frames exist. Must `request_keyframe()` (triggers DXGI reset)
  before the wait loop.

## Input
- **macOS Cmd key**: don't send Meta/Super to server — gets stuck after
  Cmd+Tab.
- **Stuck modifier keys**: Super/Meta (macOS Cmd) gets stuck on the
  server after Cmd+Tab. Server releases all modifiers on session start;
  client does NOT send Super/Meta and releases modifiers on focus loss.
- **Cmd+R stuck keys on macOS**: Meta key is blocked but `r` keydown is
  sent, page refreshes before keyup. Fix: skip ALL keys when
  `e.meta_key()` is true. Also release modifiers on `beforeunload` and
  `blur`.
- **XFCE Super shortcuts**: removed in Docker entrypoint (conflicts
  with macOS Cmd).
- **Scroll direction**: browser `deltaY` already reflects client OS
  direction (macOS natural scroll). Do NOT negate. winit (native) has the
  opposite convention from enigo — DO negate there.
- **GNOME input**: enigo (XTest) works on GNOME when no other processes
  interfere. The previous "GNOME broken" diagnosis was caused by stale
  xdotool processes, not Mutter.
- **Stale xdotool processes**: bench code spawns `xdotool mousemove`
  loops. Always `pkill -f xdotool` after bench testing — leftover loops
  send random mouse coordinates causing phantom cursor drift.

## Adaptive bitrate
- **ABR spiral on high-latency links**: previous ABR decreased bitrate
  whenever RTT >100ms (fixed latency). Fix: track baseline RTT (minimum
  observed), only decrease when RTT rises >50% above baseline (actual
  congestion).

## Autologin mode (Linux VM)
- **"Switch User" backgrounds the session**: clicking GNOME's Switch
  User menu entry doesn't terminate horde's X session — it backgrounds
  it on one VT while spawning a greeter on another. phantom stays
  pinned to `DISPLAY=:0` (the backgrounded session) and keeps streaming
  a black screen; autologin can't recover because the session isn't
  technically dead. Fix: `install.sh --autologin` sets
  `org.gnome.desktop.lockdown.disable-user-switching=true` so the menu
  entry is hidden.
- **GDM 42 TimedLogin regression**: on Ubuntu 22, `TimedLogin` doesn't
  reliably fire after sign-out; GDM sits at the greeter forever. A
  systemd watchdog timer polls every 30s and kicks `gdm3` if no
  `$TARGET_USER seat0` session exists.
- **phantom-server survives gnome-session exit**: when launched from an
  XDG autostart `.desktop`, phantom-server can get reparented to init
  (PPID=1) when gnome-session dies, keeping ports 9900/9901 bound even
  after the user's session ends. New session's autostart then silently
  fails to bind. Fix: the autostart `Exec=` wrapper pkills any existing
  phantom-server before launching its own.
- **Keyring popup under autologin**: no password captured at login →
  `pam_gnome_keyring` can't unlock → first app to use secret storage
  (Chrome, Evolution) pops a dialog. Fix: install.sh clears
  `~/.local/share/keyrings/` and drops an autostart hook that unlocks
  with empty password via `gnome-keyring-daemon --unlock <<< ""`.
  Trade-off: stored secrets are effectively plaintext.

## Service mode (Windows)
- **Windows IPC pipe deadlock**: synchronous named pipes only allow ONE
  pending I/O per handle. Concurrent `ReadFile` + `WriteFile` on the same
  DUPLEX handle deadlocks. Fix: two unidirectional pipes
  (`PhantomIPC_up` / `PhantomIPC_down`).
- **Windows agent SYSTEM token**: `WTSQueryUserToken` gives a user token
  which can't access Winlogon desktop (lock screen). Use the service's
  own SYSTEM token + `SetTokenInformation(TokenSessionId)` like Sunshine.
- **VDD on headless GPU VMs**: data center GPUs (L40, A40) in TCC mode
  have no display. VDD creates a virtual display. Must switch to WDDM
  (`nvidia-smi -fdm 0`) for VDD to render on GPU.
- **DO NOT disable Basic Display Adapter**: causes Windows boot failure.
  Even with NVIDIA WDDM, Windows needs Basic Display during early boot.
  DXGI targets VDD by device name instead.
- **IPC dead thread detection**: `is_connected()` must check
  `JoinHandle::is_finished()`. Raw `connected` bool stays true after IO
  threads die.
- **Service mode clipboard/paste**: Session 0 has no clipboard access.
  Paste: `MSG_PASTE_TEXT` IPC → agent `enigo.text()`. Clipboard sync:
  agent polls arboard → `MSG_CLIPBOARD_SYNC` IPC → `ClipboardSync` to
  client.
- **Toast JS eval + Windows paths**: backslashes in `C:\Users\...` break
  JS eval. Must escape `\\` before `\'` and `\"`.
