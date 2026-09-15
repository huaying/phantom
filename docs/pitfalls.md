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
- **WSS fallback mode**: browser defaults to WebRTC. Use `?wss` or `?ws`
  to force the older WebSocket path when debugging or when UDP is blocked.
- **WebRTC autoplay policy**: browser may reject media `play()` before
  user interaction (`NotAllowedError`). Keep the retry-on-gesture path
  wired, especially for audio.
- **Media/Control split**: video/audio are media tracks; input/control are
  DataChannels. Do not route `Message::VideoFrame` through control DC again.
- **SRTP and SRTCP use different session keys**: DTLS exports one master key
  block, but RFC 3711 derives RTP keys with labels `0/1/2` and RTCP keys with
  labels `3/4/5`. Reusing the RTP cipher for Sender Reports lets media decode
  while Chrome rejects every RTCP packet with `Failed to unprotect RTCP`.
  Keep separate RTP/RTCP crypto contexts and encrypt AES-CM SRTCP payloads.
- **SRTP must advance its rollover counter**: the 16-bit wire sequence wraps
  independently for audio and video. Keep a 48-bit packet index per SSRC and
  include ROC in the GCM nonce and AES-CM IV/authentication input. A fixed ROC
  silently freezes media after the first wrap while ICE stays connected.
  Retransmissions must reuse cached ciphertext without allocating a new index.
  Short browser smoke can miss this; validate more than 65,536 packets and
  include deterministic encryption vectors at ROC 0, 1, and 2.
- **WSS idle reads can throttle outgoing media**: four writes followed by a
  50 ms blocking read barely support 80 messages/s, before Windows timer
  rounding. Main-channel audio already consumes 50 packets/s. Keep input
  polling nonblocking, with a 5 ms sleep only when neither direction makes
  progress, while retaining the bounded queue and read/write
  fairness; raising the queue depth only hides stale video.
- **Do not arm a WSS recovery fence before its handshake**: focus and visibility
  events can arrive while WebSocket is CONNECTING or before Hello. A dropped
  RequestKeyframe followed by `waiting_for_keyframe_fence = true` discards every
  later frame indefinitely. Require a ready channel/Hello and successful send;
  cover a forced focus event at readyState 0 in the browser regression.
- **Avoid short Windows socket receive timeouts for WSS polling**: the Win11
  canary returned error 997 during a 5 ms SO_RCVTIMEO read and the stream closed.
  The single-owner TLS socket now polls reads in nonblocking mode and restores
  blocking mode before bounded writes. Do not simply ignore unknown native
  errors or retry an entire partially written WebSocket frame.

- **Recover after a transient full WSS queue**: once a delta is lost, keep
  discarding dependent deltas, but request a keyframe through the session event
  path when the I/O owner has drained the queue. Waiting for a periodic IDR
  can collide with the two-second backlog deadline and disconnect an already
  recovered socket. Coalesce requests and retain all existing queue/write
  limits; exercise both recovery and persistent-blockage tests.
- **WSS audio belongs to one connection generation**: close the previous
  audio socket, decoder and AudioContext, and clear the BufferSource drain
  timer on reconnect. Late AudioWorklet module completion must check its
  generation before installing a decoder. Test both SAB and fallback paths;
  a new audible context does not prove that the old silent context stopped.
- **An accepted audio socket is not a routed audio socket**: the Windows
  service did not consume the global audio-socket receiver, so audio stayed
  behind video on the main WSS connection. Pair the side channel using the
  current Hello's random session token and route it in the main WsSender.
  The session owns its audio socket; unknown, duplicate and ended-session
  attachments must fail, and closure must preserve main-channel fallback.
  Capture-side `audio_drops_5s=0` alone does not prove timely browser playout.
- **A fixed 60ms WSS audio prefill can empty on WAN bursts**: paired audio
  removed video head-of-line contention but did not prevent a measured SAB
  underflow. Use the main socket's monotonic connection-setup duration as a
  bounded startup hint (60–300ms, in 20ms steps), chosen before playback.
  This is not an RTT/jitter measurement. Preserve the 60ms LAN floor and the
  500ms ring bound; replay recorded arrivals through the actual worklet and
  retain a long-outage control that must still produce observable silence.
- **A full SAB ring must not overwrite unplayed PCM**: check whole-packet
  capacity before writing. Drop a packet that cannot fit, publish its buffered
  count only after writing, and leave read-position ownership to the consumer.
  Writing first and capping the shared count both corrupts queued samples and
  can lose a concurrent consumer decrement. Exercise the real WASM producer
  with marked AudioData at full/partial capacity and across ring wrap.
- **Cursor subscriptions belong to the server session**: reset the browser's
  cursor-state and cursor-shape subscription flags when WSS closes and when
  RTC runtime is reset. Otherwise
  the next Hello skips both opt-ins and a cached arrow survives reconnect even
  over a Windows text box. Test both transports after in-place reconnect;
  a full page reload hides this bug.
- **WebRTC session zombie**: ICE may remain alive after the shared session loop
  replaces RTC with WSS. Detect disconnected video/audio/control bridge
  receivers and terminate the old backend; otherwise it sends UDP keepalives
  and `rtc-stats` forever with no media owner.
- **Sender Reports must advance during a static desktop**: RTCP SR RTP and NTP
  timestamps describe the same instant (RFC 3550 section 6.4.1). Reusing the
  last video packet's RTP timestamp with a fresh NTP timestamp falsely reports
  a stalled media clock. Extrapolate the video clock at report time; for audio,
  extrapolate from the latest packet's timestamp and send instant. Initial
  decoding alone does not verify this: leave the desktop idle, then drag a
  window and check presentation latency as well as decoded-frame counts.
- **ICE consent and UDP source nomination are authenticated**: never update the
  active peer from an arbitrary UDP packet. Validate both the ICE username and
  STUN MESSAGE-INTEGRITY before accepting a source or refreshing consent, then
  stop application traffic after 30 seconds without a valid Binding request.
  This prevents both zombie sessions and unauthenticated media redirection.
- **WSS same port**: WS upgrade lives on HTTPS port 9900 (not separate
  port). Avoids self-signed cert rejection for a second port.
- **HTTP query string**: strip `?ws` from path before routing — `/?ws`
  returns 404 otherwise.
- **WSS recovery keyframes need a realistic socket write budget**: Winlogon's
  software H.264 IDR can be hundreds of KiB. A 150ms TLS write timeout tore
  down healthy WAN connections halfway through the keyframe. Keep the socket
  write budget aligned with the two-second bounded-queue backlog guard; do not
  weaken the queue bound to solve this.
- **WSS loss must respect H.264 GOP boundaries**: the bounded send queue stops
  unbounded growth, but dropping an arbitrary P-frame corrupts all dependent
  frames until the next IDR. After the first full queue, drop delta frames
  until a keyframe can be queued; close a connection whose writer remains
  blocked for two seconds so browser/kernel TCP buffers cannot fast-forward.
- **HTTPS required for WebCodecs**: non-localhost HTTP is not a secure
  context. Server uses self-signed TLS (rcgen) for HTTPS.
- **QUIC ALPN mismatch**: server sets `alpn_protocols = ["phantom"]` but
  client must also set it. Without matching ALPN, TLS handshake fails
  with "peer doesn't support any known protocol". Fixed in e4487ec.

## Audio capture
- **WASAPI silence is a flag, not sample data**: when a packet has
  `AUDCLNT_BUFFERFLAGS_SILENT`, generate the reported duration of zeros before
  inspecting its pointer. The supplied memory need not contain valid silence.
  Reading it can forward stale audio or dereference an unusable pointer. Reject
  a null non-silent packet after releasing the capture buffer. Keep Windows
  regression coverage for null silent data, stale nonzero data with the silence
  flag, and ordinary PCM. See
  [Microsoft's capture example](https://learn.microsoft.com/en-us/windows/win32/coreaudio/capturing-a-stream).
- **Device gaps precede transport buffering**: a packet counter below the
  expected sample cadence can originate at the render endpoint. Compare
  WASAPI packet positions and an independent renderer's buffer/clock before
  changing RTC or WSS queues. The testing runbook includes a metadata probe;
  receiving nonzero audio is not sufficient audio-quality acceptance.
- **A small WASAPI buffer can add a second failure**: on the Win11 AWS speaker
  fixture, disabling the defective endpoint event mode restored its render
  clock, but the old 20 ms polling capture buffer still overflowed. A 100 ms
  buffer removed those capture gaps. Buffer capacity does not add a fixed
  100 ms playback delay: drain immediately and retain 20 ms Opus packetization.
  The endpoint compatibility repair is explicit and reversible; see the
  testing runbook and `scripts/windows-audio-polling.ps1`.

## Capture / encode (GPU)
- **NVDEC ABI and readiness**: CUVID packet lengths/flags are C `unsigned long`
  (8 bytes on Linux). In SDK 12.2, parser userdata/sequence/decode/display
  callbacks start at offsets 40/48/56/64, and `CUVIDPROCPARAMS` is 264 bytes.
  Incorrect layouts can let parser initialization succeed while producing no
  frames. Verify RGB pixels, not initialization or accepted-packet counters.
  Native FPS must count nonempty decoder output. Mark complete access units
  with ENDOFPICTURE so a static desktop's last picture is not buffered.
  Rebuild decode surfaces from sequence coded dimensions/crop and the required
  surface count; Hello dimensions alone are insufficient after a resize.
  Preserve the caller CUDA context and release parser/decoder/context on both
  successful teardown and constructor errors. `nvdec_smoke` checks actual
  color pixels through five resolutions and context restoration.
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
- **NVFBC desktop resize**: capture dimensions can change while the server
  remains running, including while no viewer is connected. Encoding the new
  NV12 buffer with the old encoder height offsets the chroma plane and
  produces green/torn video. Validate the frame's NV12 layout and rebuild
  NVENC for its actual dimensions after releasing the NVFBC context. Before
  Hello, reset capture and retain a fresh NOWAIT frame to establish current
  dimensions. Handle `NVFBC_ERR_MUST_RECREATE` by recreating capture. A normal
  reconnect at unchanged dimensions only forces an IDR; it must not recreate
  the encoder. Restore its configured bitrate between sessions so a new ABR
  controller does not multiply its ceiling from the preceding adapted value.
- **RTC resize input mapping**: a media-track video changes dimensions through
  SPS/PPS without a second Hello. Handle the video element's `resize` event
  even when its first frame is already ready; update the input overlay and
  remote dimensions from `videoWidth`/`videoHeight`. Otherwise a 720p-to-1080p
  switch renders correctly but pointer coordinates remain scaled by 2/3.
- **Windows installer on Winlogon**: automatic/managed topology changes are
  deferred until Default so LogonUI keeps its original surface. Doctor must
  accept a fresh keyframe committed to the current Winlogon session, with a
  running service and browser listener, while reporting the policy deferral
  as a warning. An agent launch, capture-tier selection, stale log, or commit
  for an older session is insufficient. Wait for policy/secure-capture
  readiness, not just an IPC connection, before evaluating doctor results.
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
- **Keyframe retries must NOT reset DXGI**: periodic timers and the service's
  repeated startup requests only set `force_idr` and issue a throttled desktop
  repaint. Recreating Desktop Duplication for every retry (or every rejected
  black transition frame) can prevent the first real Default-desktop frame from
  ever arriving. Recreate only after a topology transition or the bounded
  startup timeout.
- **OpenH264 SIMD**: must use `phantom_core::color::bgra_to_yuv420`
  (AVX2 SIMD), NOT `pixel_f32()` callback. Per-pixel f32 = ~300ms/frame
  at 1080p; SIMD = ~10ms.

## Decode (client)
- **Native clipboard I/O must not run on the UI thread**: on macOS,
  `NSPasteboard` may wait for another application's lazy data provider. A
  synchronous `get_text()` then freezes decode, rendering, and input. Keep all
  arboard reads/writes in the clipboard worker and pass results over channels.
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
- **Bounded channels**: WSS and WebRTC bridge queues stay bounded to avoid
  stale backlog replay. A lossy video queue must recover at an IDR boundary.
- **IPC encoded frames must be sequential**: H.264 P-frames depend on previous
  frames. Never `try_send`, drain-to-latest, or skip an arbitrary IPC frame;
  backpressure the local pipe and forward every queued frame in order.
- **Keyframe request must come BEFORE wait-for-frame loop**:
  `create_service_session` waits for a decodable first frame. On a static
  desktop no update may exist, so request an IDR and a throttled repaint before
  waiting; do not reset DXGI on each retry.

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
- **Manual screen lock traps autologin user**: if the autologin user
  has no Unix password (typical for VMs that only ever auth via SSH
  keys), and they hit the lock corner / Super+L / idle lock, GNOME's
  unlock dialog requires the account password they don't have. The
  remote viewer sees a stuck lock screen and cannot recover without
  out-of-band SSH access to set a password. `install.sh --autologin`
  disables GNOME lock/idle settings and drops XFCE/light-locker
  autostart overrides so an idle lock cannot switch the active seat to a
  display-manager greeter and make NVFBC capture a black/backgrounded
  desktop.

## Service mode (Windows)
- **Windows IPC pipe deadlock**: synchronous named pipes only allow ONE
  pending I/O per handle. Concurrent `ReadFile` + `WriteFile` on the same
  DUPLEX handle deadlocks. Fix: two unidirectional pipes
  (`PhantomIPC_up` / `PhantomIPC_down`).
- **Windows agent SYSTEM token**: `WTSQueryUserToken` gives a user token
  which can't access Winlogon desktop (lock screen). Use the service's
  own SYSTEM token + `SetTokenInformation(TokenSessionId)` like Sunshine.
- **Winlogon keyboard input requires scan codes**: `SendInput` can report
  success for virtual-key events while the Windows credential UI ignores
  them. Map Phantom key codes with `MapVirtualKeyW` and send
  `KEYEVENTF_SCANCODE` (plus `KEYEVENTF_EXTENDEDKEY` where required), falling
  back to virtual keys only when no scan code exists. Test by entering an
  actual password on Winlogon; receiving the event in the agent log is not
  sufficient proof.
- **VDD on headless GPU VMs**: data center GPUs (L40, A40) in TCC mode
  have no display. VDD creates a virtual display. Must switch to WDDM
  (`nvidia-smi -fdm 0`) for VDD to render on GPU.
- **DO NOT disable Basic Display Adapter**: causes Windows boot failure.
  Even with NVIDIA WDDM, Windows needs Basic Display during early boot.
  DXGI targets VDD by device name instead.
- **No-GPU Windows VM fallback order**: on Basic Display Adapter / no
  `nvEncodeAPI64.dll`, `ScrapCapture` may initialize successfully but
  never produce a frame. Do NOT assume "scrap is the safer CPU path" on
  Windows — on the tested Win11 no-GPU VMs, GDI was the reliable
  fallback. Agent must switch to GDI immediately on Scrap stall instead
  of re-entering a Tier-2 reinit loop.
- **Display topology needs one owner**: enabling VDD, changing primary, or
  choosing another output while an external manager owns the desktop can move
  open windows onto an inaccessible display. Detect a running DCV service via
  the Windows Service API, adopt its active target, and do not mutate topology.
  A dedicated Phantom-managed host may provision one sole VDD before capture;
  subsequent capture recovery stays on that target. Adaptive resize and origin
  repair both refuse multi-display topology.
- **CCD readiness must match the DXGI surface**: an active target name alone is
  insufficient during login transitions. Windows may briefly report an old
  640x480/800x600 duplication surface while CCD already says 1920x1080. Reject
  the candidate until dimensions agree, then rebuild DXGI instead of committing
  a scaled, duplicated, or partially black desktop.
- **Winlogon topology is Windows-owned**: after a real sign-out, Windows may
  create a new 800x600 secure-desktop surface even when the previous Default
  desktop used a 1920x1080 VDD. Re-provisioning CCD after LogonUI starts can
  produce a nominal 1920x1080 GDI frame with only an 800x600 login screen in
  one corner. Capture Winlogon from its primary desktop DC using
  `GetSystemMetrics`, not the possibly stale CCD device rect, and defer
  Phantom-owned VDD provisioning or resize until Default is active. A normal
  lock still reports the already-active VDD dimensions through those desktop
  metrics.
- **An explicit DXGI target is strict**: VDD may be installed but inactive. If
  that target is absent, never silently capture the first NVIDIA/physical
  output and then label it managed VDD. Either capture the immutable active
  target or keep the previous generation visible while preparing a replacement.
- **Agent replacement is prepare-then-commit**: IPC connection only proves that
  the process started. A candidate must produce a non-empty keyframe before it
  replaces the committed generation. During sign-out, Windows can briefly
  report the old session/desktop before allocating a new console session;
  cancel that stale candidate immediately instead of waiting its full timeout.
  The old session may also reject every new process with `0xC0000142`; use
  bounded `1s -> 2s -> 4s` retry backoff for the same target, but reset it as
  soon as the observed session or desktop changes.
- **Generation-scoped IPC prevents cross-agent races**: old and candidate
  agents cannot share pipe names. Restrict each pipe ACL to SYSTEM,
  administrators, and the target logon SID; do not grant all interactive users.
- **Never infer frame validity from compressed H.264 size**: a static login
  screen or clean desktop can produce a legitimately tiny IDR. Rejecting such a
  frame as "suspicious" can create the black transition it was meant to avoid.
  Validate DXGI startup with sampled source pixels and require an IDR at the
  service boundary; do not apply a byte-per-pixel threshold to encoded data.
- **Do not continuously relocate arbitrary application windows**: keeping a
  physical display active beside VDD means Windows may remember positions on
  either path. A periodic `EnumWindows`/`SetWindowPos` sweep mutates the user's
  local layout and still cannot contain shell surfaces or partial windows.
  Treat multi-display ownership as an explicit topology/product decision.
- **Resolution hint mismatch can delay first frame by ~8s**: on
  no-GPU/GDI fallback, the session may settle at 1280x800 while the
  client initially asks for 1920x1080. `create_service_session()`
  should not wait the full timeout discarding stale frames forever; it
  needs a last-seen-frame fallback so the session starts with the actual
  working resolution.
- **IPC dead thread detection**: `is_connected()` must check
  `JoinHandle::is_finished()`. Raw `connected` bool stays true after IO
  threads die.
- **Service mode clipboard/paste**: Session 0 has no clipboard access.
  Paste: `MSG_PASTE_TEXT` IPC → agent `enigo.text()`. Clipboard sync:
  agent polls arboard → `MSG_CLIPBOARD_SYNC` IPC → `ClipboardSync` to
  client.
- **Toast JS eval + Windows paths**: backslashes in `C:\Users\...` break
  JS eval. Must escape `\\` before `\'` and `\"`.
- **`Import-Certificate` E_ACCESSDENIED on fresh Windows images** (FIXED
  in `install_vdd`): on a freshly-provisioned Windows VM that has never
  opened certlm.msc / certutil, the registry key
  `HKLM:\SOFTWARE\Microsoft\SystemCertificates\TrustedPublisher`
  *does not exist* — Windows initialises only `Root`, `MY`, `Disallowed`,
  etc. by default. `Import-Certificate -CertStoreLocation Cert:\LocalMachine\TrustedPublisher`
  then fails with E_ACCESSDENIED *even when running as NT AUTHORITY\SYSTEM*,
  because the underlying store can't be opened for write. This used to
  be misdiagnosed as a TCC→WDDM GPU-mode-transition race ("first
  `--install` silently falls back to CPU because cert import fails
  during mode switch"); both the symptom and the eventual recovery
  (reboot + `--uninstall` + `--install`) match because some Windows
  service initialises the missing key after first boot. The actual
  fix is `New-Item -Path $key -Force` before `Import-Certificate`,
  applied in `service_win.rs::install_vdd`.
  See: https://learn.microsoft.com/en-us/answers/questions/1679945/
- **`phantom-server.exe --install` re-run while service is running fails**:
  the service holds an exclusive lock on `C:\Program Files\Phantom\phantom-server.exe`,
  so the install's "copy exe to install dir" step errors with
  `os error 32 (file in use)`. `sc stop PhantomServer` may also hang
  (state stays RUNNING after an `sc stop`) because the Session 1 agent
  child process doesn't shut down on the SCM stop signal. Reliable
  recovery: `sc stop`, then `Stop-Process -Force phantom-server`, then
  `--uninstall`, then `--install`. Worth wiring into `--install` as
  an automatic "kill stale processes" step.
- **VDD virtual display defaults to 640x480**: even though `--install`
  prints `Virtual Display Driver installed (1920x1080 default)`, the
  agent log reports `display[0] 640x480 primary=true` and `Tier 1
  DXGI→NVENC ready 640x480`. The real session resolution then comes
  from the client's `resolution hint`. Investigate whether the
  "1920x1080 default" log line is misleading or the VDD config isn't
  actually being applied.

### Native audio output survives reconnect

The native client previously forgot its CPAL stream and left an endless monitor
thread per connection. A real reconnect on an isolated PulseAudio null sink
increased active outputs from one to two. Playback now belongs to the session:
dropping it stops CPAL, closes the Opus channel and joins the decoder/monitor
workers. Initialization failures use the same cleanup. Validate repeated native
reconnects by checking one active output, stopped old workers and continuous
known-tone PCM; successful audio initialization alone cannot detect this leak.
