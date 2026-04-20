# Phantom — Feature Reference

Accurate as of v0.4.10. Each entry points at the code that implements it.

## Transports

| Transport | Default | File | Notes |
|---|---|---|---|
| TCP (plain + ChaCha20-Poly1305) | ✅ | `server/src/transport/tcp.rs` | Native client; encryption on unless `--no-encrypt` |
| WebSocket over HTTPS | ✅ | `server/src/transport/ws.rs` | Browser client, embedded WASM, same port as HTTPS static |
| QUIC | opt-in `--transport quic` | `server/src/transport/quic.rs` | Native client only; self-signed TLS, no head-of-line blocking |
| WebRTC DataChannel | feature `webrtc` | `server/src/transport/webrtc.rs` | str0m 0.18; 16KB SCTP limit → chunking; opt-in at build time |

All implement `MessageSender` + `MessageReceiver` (`core/src/transport.rs`); session loop is transport-agnostic.

## Capture

| Backend | Flag | Platform | File |
|---|---|---|---|
| scrap (GDI / XCB) | `--capture scrap` or default fallback | Linux X11, Windows, macOS | `server/src/capture/scrap.rs` |
| NVFBC (NVIDIA GPU zero-copy) | `--capture nvfbc` | Linux + NVIDIA | `gpu/src/nvfbc.rs` |
| DXGI (Desktop Duplication) | used with `--encoder nvenc` on Windows | Windows | `gpu/src/dxgi.rs` + `gpu/src/dxgi_nvenc.rs` |
| GDI | Service agent fallback | Windows Session 0 / lock screen | `server/src/capture/gdi.rs` |
| PipeWire + XDG Portal | `--capture pipewire` | Linux Wayland (feature `wayland`) | `server/src/capture/pipewire.rs` |

`--capture auto` probes GPU via `gpu_probe::best_capture()` then falls back to scrap.

## Encoding

| Encoder | Codec | File | Notes |
|---|---|---|---|
| OpenH264 CPU | H.264 | `server/src/encode/h264.rs` | Baseline, always available |
| NVENC | H.264 + AV1 | `gpu/src/nvenc.rs` | Runtime dlopen, no build-time CUDA dep |
| NVENC fused with DXGI | H.264 + AV1 | `gpu/src/dxgi_nvenc.rs` | Zero-copy D3D11 texture → NVENC |

`--codec auto` → H.264 (default for client compatibility). `--codec av1` opts into AV1 on Ada+ NVENC. Periodic keyframe every 2s; keyframe on client reconnect; keyframe on input burst.

### AV1 status — opt-in, known decode problems

AV1 works server-side on Ada Lovelace+ NVENC (hardware encode ~2.5ms).
The **client decode side is the weak link**, which is why `--codec auto`
picks H.264 and AV1 is opt-in via `--codec av1`:

- **Web browser OOM crash** — WebCodecs software AV1 decode at 1080p30
  pushes 30-50% CPU and can OOM the browser tab (reproducible L40 →
  Safari/Chrome on Mac).
- **Native client laggy typing** — dav1d software decode ~20-40ms per
  1080p frame on a mid-range Mac → end-to-end feels like laggy typing.

Hardware AV1 decode fixes both, but coverage is patchy: macOS
VideoToolbox has no AV1 support on M2 and earlier; NVDEC AV1 needs
Turing/Ampere/Ada depending on the profile; browser hardware AV1 depends
on OS + GPU + browser combo.

Until codec negotiation in `ClientHello` (task #34) lets the server
know what the client can actually decode, AV1 defaults off. Pass
`--codec av1` on a server where you control the clients and know they
have hardware AV1 decoders.

## Decode (client)

| Decoder | File | Platform |
|---|---|---|
| OpenH264 CPU | `client/src/decode_h264.rs` | All |
| dav1d (AV1) | `client/src/decode_av1.rs` | All (feature `av1`) |
| NVDEC | `gpu/src/nvdec.rs` | Linux + Windows NVIDIA (feature `nvdec`) |
| VideoToolbox | `client/src/decode_videotoolbox.rs` | macOS (H.264 only) |
| WebCodecs | `web/src/lib.rs` | Browser |

Probe order: `--decoder auto` picks NVDEC → VideoToolbox → dav1d → OpenH264.

## Pipeline abstraction (0.4.7)

Unified trait in `server/src/pipeline.rs`. Three impls share one session loop via `session::run_session(&mut dyn Pipeline, cfg)`:

| Pipeline | Backing | Platform |
|---|---|---|
| `CpuPipeline` | scrap + OpenH264 / NVENC + `TileDiffer` | All |
| `NvfbcNvencPipeline` | NVFBC → NVENC, CUDA zero-copy | Linux GPU |
| `DxgiNvencPipelineAdapter` | fused `gpu::dxgi_nvenc::DxgiNvencPipeline` | Windows GPU |

Session logic (input, clipboard, audio, keepalive, stats, adaptive bitrate, congestion, file transfer, frame pacing) lives once in `run_session`.

## Audio

- **Capture**: PulseAudio monitor (`server/src/audio/pulse.rs`, Linux) / WASAPI loopback (`server/src/audio/wasapi.rs`, Windows). Feature `audio`, default on.
- **Codec**: Opus 48kHz stereo, 20ms frames (static libopus via `audiopus_sys` feature `static`).
- **Playback**: Browser via WebCodecs + Web Audio API; native via `cpal`.
- **Sent audio-first** inside the session loop so video backpressure never blocks the ~100-byte audio packets.

## Input

| Backend | Purpose | File |
|---|---|---|
| uinput virtual keyboard | Linux keyboard (bypasses GDM 42 XKB remap scramble, works on Wayland + lock screen) | `server/src/input_uinput.rs` |
| enigo | Cross-platform keyboard + mouse + scroll fallback | `server/src/input_injector.rs` |
| IPC forwarder | Windows Service → agent (in user session) | `server/src/ipc_pipe.rs` |

uinput needs a udev rule + `input` group membership — `install.sh` wires both.

## Clipboard

Bidirectional text sync via `arboard` (server + native client) and Async Clipboard API (browser). Echo suppression via `ClipboardTracker` (`core/src/clipboard.rs`). Ctrl+V paste injection forwards as `PasteText` so the server types the string character-by-character.

## File transfer

Bidirectional, chunked, SHA-256 verified. Server → client via `--send-file <path>`; client → server via `--send-file <path>`. Protocol: `FileOffer` → `FileAccept` → `FileChunk*` → `FileDone` (`core/src/file_transfer.rs`, `server/src/file_transfer.rs`, `client/src/file_transfer.rs`).

## Session lifecycle

- **Hello**: server sends resolution, codec, audio flag, opaque `session_token`.
- **Resume**: client reconnects with `(session_token, last_sequence)`; server replies `ResumeOk` + forces keyframe, else a fresh `Hello`.
- **Replacement**: new client takes over an active session; ghost-set rejects the old client's auto-reconnect attempts.
- **Disconnect**: server pushes `Message::Disconnect { reason }` on graceful shutdown / Ctrl+C / SIGTERM.

Session end reasons are structured (`SessionEndReason`: `Cancelled`, `PeerClosed`, `ClientDisconnect`, `NetworkError`, `PipelineError`) so ops-side log parsing is stable.

## Adaptive bitrate + congestion

- `AdaptiveBitrate` (`server/src/session.rs`): RTT-aware, ×0.7 on high RTT, ×1.2 on stable, clamped to [min, max], hysteresis ≥5s.
- `CongestionTracker`: counts slow frames; once a threshold is hit, skips 1/N frames to recover; releases when frames land on time.
- GPU pipelines expose no `CongestionTracker` (can't usefully skip after zero-copy encode). CPU pipeline does.

## Logging

`--log-file <path>` + `--log-rotate <daily|hourly|never>` + `--log-keep <N>`. Stats line every ~5s:

```
session_id=<8-char hex>  fps=X.X  bw=X.X KB/s  rtt=Xms  jitter=Xms  encode_ms=X.X  audio_drops_5s=N
```

Structured fields via `tracing`; stdout + file if `--log-file` set.

## Network / deployment

- **STUN**: `--stun auto` (Google public) or `--stun <server>` prints a connection code with discovered public `ip:port`. `--public-addr ip:port` skips STUN if you already know the externally-reachable address.
- **JWT auth (WSS)**: `--auth-secret <hex>` turns on HMAC-SHA256 JWT verification; browser supplies `?token=<jwt>` on WebSocket URL. No token support on TCP/QUIC.

## Windows Service mode

`--install` / `--uninstall` / `--install-vdd` (`server/src/service_win.rs`):

- Registers `PhantomServer` Windows Service (runs in Session 0, SYSTEM, pre-login)
- Downloads + installs [MTT Virtual Display Driver](https://github.com/VirtualDrivers) via nefcon (so headless GPU servers have something for DXGI to capture)
- Service spawns an agent in the active console session via `CreateProcessAsUser`; agent does DXGI capture + enigo injection, service relays frames over two named pipes (`\\.\pipe\PhantomIPC_{up,down}_{session_id}`)
- `--install-vdd` re-runs just the VDD step if a transient download blip killed the first attempt

## Linux VM autologin mode

`install.sh server --autologin` (`install.sh`):

- GDM `AutomaticLogin` + `TimedLogin` (5s) — session auto-restores after sign-out
- dconf overrides disable screen lock, idle, and the GNOME "Switch User" menu item (that menu entry backgrounds the session and breaks phantom's DISPLAY binding)
- Clears + re-seeds keyring with empty password so Chrome/Evolution don't pop a keyring dialog under autologin
- Drops an XDG autostart `.desktop` for phantom-server (with wrapper that kills any stale instance first, since phantom-server sometimes survives gnome-session exit and blocks port 9900/9901)
- Installs a systemd timer watchdog that polls every 30s and kicks `gdm3` if no horde seat0 session exists — workaround for GDM 42's `TimedLogin` regression on Ubuntu 22

## CLI reference

### Server (`phantom-server`)

```
--listen <addr>                        default 0.0.0.0:9900
--transport <tcp,web,quic>             comma-separated; default tcp,web
--fps <n>                              default 30
--bitrate <kbps>                       default 5000; seeds ABR
--encoder <auto|openh264|nvenc>        default auto
--codec <auto|h264|av1>                default auto (→ H.264)
--capture <auto|scrap|nvfbc|pipewire|dxgi>
                                       default auto
--display <n>                          display index; --list-displays to enumerate
--send-file <path>                     push file to first client
--key <hex> | --no-encrypt             ChaCha20 key / disable
--stun <server|auto>                   NAT discovery
--public-addr <ip:port>                override STUN
--auth-secret <hex>                    HMAC-SHA256 for JWT auth over WSS
--log-file <path> --log-rotate <daily|hourly|never> --log-keep <n>
                                       production logging
Windows only:
--install / --uninstall / --install-vdd
--agent-mode / --service / --ipc-session
                                       internal use
```

### Client (`phantom-client`)

```
-c, --connect <addr:port>              default 127.0.0.1:9900
--transport <tcp|quic>                 default tcp
--decoder <auto|openh264|videotoolbox>
                                       default auto. dav1d / nvdec are not
                                       explicit flag values — on non-macOS,
                                       `auto` auto-probes NVDEC first then
                                       dav1d (if AV1) then OpenH264. On
                                       macOS, `auto` and `videotoolbox`
                                       use VideoToolbox; anything else
                                       falls through to OpenH264.
--send-file <path>                     push file to server
--key <hex> | --no-encrypt             must match server
--token <jwt>                          for WSS JWT auth
```
