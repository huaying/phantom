# Phantom — File Map

After the 0.4.0 reorg, server modules cluster by concern.

```
crates/core/src/                  Cross-platform foundation, zero cfg
  lib.rs                          Module exports
  capture.rs                      FrameCapture trait
  encode.rs                       FrameEncoder + Encoder (tile) + FrameDecoder traits
  decode.rs                       Decoder trait (tile)
  transport.rs                    MessageSender / Receiver / Connection traits
  display.rs                      Display trait
  protocol.rs                     Message enum, wire framing (bincode, length-prefixed)
  tile.rs                         TileDiffer (64x64 dirty detection, sampling fast-path)
  frame.rs                        Frame struct, PixelFormat
  input.rs                        InputEvent, KeyCode, MouseButton
  clipboard.rs                    ClipboardTracker (echo-loop prevention)
  color.rs                        BGRA↔YUV420/NV12 + NV12→RGB (BT.601, AVX2 SIMD + scalar fallback)
  crypto.rs                       ChaCha20-Poly1305 EncryptedWriter/Reader (feature-gated)
  stun.rs                         STUN client for NAT discovery
  file_transfer.rs                Shared file-transfer protocol types

crates/server/src/                Capture → encode → ship pixels
  main.rs                         CLI args, transport selection, codec/encoder/capture
                                  auto-detection, agent mode (run_agent_loop), doorbell
                                  for non-service paths (Linux + Windows console)
  session.rs                      SessionRunner: capture→encode→send loop, ABR, RTT,
                                  audio, file transfer, stats, run_session_ipc
                                  (service mode forwarding), SessionEndReason
  doorbell.rs                     Pure session-affinity decision (ghost-set policy),
                                  shared between main.rs and service_win.rs doorbells
  ipc_pipe.rs                     Named pipe IPC: two-pipe (up/down), encoded H.264
                                  frames + input events + keyframe requests
  service_win.rs                  Windows Service: SCM dispatcher, SessionManager, agent
                                  lifecycle (CreateProcessAsUser w/ SYSTEM token),
                                  install/uninstall (Windows only)
  display_ccd.rs                  Windows CCD API: VDD primary topology (Windows only)
  input_injector.rs               enigo: mouse/keyboard injection + type_text for paste,
                                  modifier release
  file_transfer.rs                Server-side file transfer handler

  audio/                          Cross-platform audio capture → Opus
    mod.rs                          dispatch + AudioChunk + AudioDropCounter
    pulse.rs                        PulseAudio monitor → Opus 48kHz stereo (Linux)
    wasapi.rs                       WASAPI loopback → Opus 48kHz stereo (Windows)

  capture/                        Screen capture
    mod.rs                          cfg gates: scrap always, gdi Windows-only,
                                    pipewire feature-gated
    scrap.rs                        ScrapCapture (impl FrameCapture, cross-platform,
                                    DXGI on Windows)
    gdi.rs                          GDI BitBlt for lock screen / Session 0 fallback
                                    (OpenInputDesktop + SetThreadDesktop)
    pipewire.rs                     PipeWire + XDG Desktop Portal (Wayland, feature-gated)

  encode/                         CPU encoders (GPU encoders live in phantom-gpu)
    mod.rs                          re-exports
    h264.rs                         OpenH264Encoder (impl FrameEncoder, CPU baseline)

  transport/                      All implement MessageSender/Receiver
    mod.rs                          re-exports
    tcp.rs                          TCP: Plain/Encrypted sender/receiver, split via try_clone
    quic.rs                         QUIC: quinn, self-signed TLS, keep-alive
    ws.rs                           WebServerTransport: HTTPS static + WSS upgrade
                                    (same port) + WebRTC POST /rtc + HTTP keep-alive +
                                    connection pool + JWT auth (verify_jwt)
    webrtc.rs                       str0m 0.18 run_loop, ActiveClient, chunked writes
                                    (>16KB), 1ms polling (feature `webrtc`)

  bin/mock_server.rs              Animated H.264 frames without screen capture
  tests/                          See "Tests" below

crates/client/src/                Native client (winit + softbuffer)
  main.rs                         winit ApplicationHandler, reconnect loop, borderless
                                  fullscreen, macOS transparent title bar, ClientHello
                                  with preferred_server_resolution
  display_winit.rs                softbuffer rendering, coordinate mapping, cursor
                                  overlay, macOS top-edge gradient backdrop
  input_capture.rs                winit KeyCode → phantom KeyCode, Sunshine-style
                                  scroll accumulation
  decode_h264.rs                  OpenH264Decoder (impl FrameDecoder, CPU fallback)
  decode_av1.rs                   Dav1dDecoder (AV1 software decode, uses color.rs SIMD)
  decode_videotoolbox.rs          VideoToolbox hardware decoder (macOS, Annex B→AVCC)
  decode_zstd.rs                  ZstdDecoder (impl Decoder, see TODO #24)
  audio_playback.rs               Opus decode → cpal ring buffer (300ms target,
                                  100ms prime, soft drain, underrun/trim metrics)
  file_transfer.rs                Client-side file transfer handler
  transport_tcp.rs                TCP client: Plain/Encrypted, split
  transport_quic.rs               QUIC client: quinn, skip cert verification

crates/web/src/                   Browser client (Rust → WASM)
  lib.rs                          WASM entry, setup_webrtc (POST /rtc) + setup_ws
                                  (`?ws` fallback), ChunkAssembler (reassembles
                                  >16KB DataChannel messages), WebCodecs software
                                  decode, Canvas render, got_keyframe guard,
                                  mouse/keyboard/scroll input, JWT passthrough,
                                  stuck-key prevention, ClientHello with
                                  preferred_viewport
  pkg/                            wasm-pack output (committed so Windows builds work
                                  without wasm-pack)
crates/server/web/
  index.html                      Minimal HTML loader for WASM (embedded via
                                  include_str! in transport/ws.rs)

crates/gpu/src/                   NVIDIA GPU pipeline (runtime dlopen, no build dep)
  lib.rs                          Module exports
  dl.rs                           Runtime dlopen/dlsym abstraction
  sys.rs                          C FFI types: CUDA, NVENC (SDK 12.1), NVFBC (v1.8/1.9)
  cuda.rs                         CUDA driver API: context, memory, memcpy, primary ctx
  nvenc.rs                        NvencEncoder (impl FrameEncoder): H.264 + AV1 GPU
                                  encode (uses phantom_core::color for BGRA→NV12)
  nvdec.rs                        NvdecDecoder: NVDEC hardware decode H.264 + AV1
                                  via CUVID API (feature-gated `nvdec`)
  nvfbc.rs                        NvfbcCapture (impl FrameCapture): GPU screen
                                  capture via NVFBC (Linux only)
  probe.rs                        GPU capability probe: NVENC codecs, AV1 detection
  dxgi.rs                         DxgiCapture: DXGI Desktop Duplication →
                                  ID3D11Texture2D (Windows only)
  dxgi_nvenc.rs                   DxgiNvencPipeline: DXGI capture + NVENC encode
                                  zero-copy (Windows only)

crates/bench/src/main.rs          Encoder benchmark: OpenH264 vs NVENC × resolutions

Tests
-----
crates/server/tests/              Integration tests (run in CI)
  e2e_headless.rs                 End-to-end without GUI capture
  multi_transport_test.rs         TCP + WSS + QUIC simultaneously
  pipeline_test.rs                Capture → encode → decode round trip
  quic_test.rs                    QUIC ALPN + handshake
  sctp_backpressure_test.rs       str0m SCTP buffered_amount + backpressure
  wan_test.rs                     WAN simulation: TCP proxy with delay/jitter,
                                  8 E2E tests
crates/server/src/doorbell.rs     7 unit tests (in #[cfg(test)] mod)
crates/server/src/session.rs      7 unit tests (classify_session_error, in
                                  #[cfg(test)] mod)
crates/core/src/protocol.rs       Message round-trips incl. ClientHello

Top-level
---------
Cargo.toml                        Workspace members
.github/workflows/                CI + release pipelines
Dockerfile + docker-entrypoint.sh + docker-compose.yml
README.md                         Public-facing
CLAUDE.md                         AI-specific instructions, points at docs/
docs/                             Reference documentation (this dir)
install.sh / install.ps1          One-shot installers (download from Releases)
LICENSE                           MIT
```
