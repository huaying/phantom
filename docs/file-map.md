# Phantom — File Map

```
crates/core/src/                  Cross-platform foundation, no cfg gates
  lib.rs                            Module exports
  capture.rs                        FrameCapture trait
  encode.rs                         FrameEncoder, FrameDecoder, VideoCodec, EncodedFrame
  transport.rs                      MessageSender / MessageReceiver / Connection traits
  protocol.rs                       Message enum, PROTOCOL_VERSION=7, bincode framing
  tile.rs                           TileDiffer (64x64 dirty detection, AVX2 fast path)
  frame.rs                          Frame struct, PixelFormat
  input.rs                          InputEvent, KeyCode, MouseButton
  clipboard.rs                      ClipboardTracker (echo-loop prevention)
  color.rs                          BGRA↔YUV420/NV12 + NV12→RGB (BT.601, AVX2 + scalar)
  crypto.rs                         ChaCha20-Poly1305 EncryptedWriter / Reader (feature-gated)
  stun.rs                           STUN client for NAT discovery
  file_transfer.rs                  Shared file-transfer protocol types

crates/server/src/                Capture → encode → ship pixels
  lib.rs                            Re-exports modules so integration tests can
                                    reach them (0.4.5 lib+bin split)
  main.rs                           CLI, transport selection, codec/encoder/capture
                                    auto-detection, agent mode, doorbell wiring
  session.rs                        SessionRunner (per-session state + helpers),
                                    run_session generic loop, entry-point shims
                                    (run_session_cpu / _gpu / _dxgi / _ipc)
  pipeline.rs                       Pipeline trait + 3 impls: CpuPipeline,
                                    NvfbcNvencPipeline, DxgiNvencPipelineAdapter
                                    (0.4.7 — unifies the 3 capture+encode backends)
  doorbell.rs                       Session-affinity policy: ghost-set LRU prevents
                                    kicked browser tabs from thrashing real sessions
  ipc_pipe.rs                       Windows-only: named-pipe IPC between service
                                    (Session 0) and agent (user session), up + down
  service_win.rs                    Windows Service: SCM dispatcher, SessionManager,
                                    agent lifecycle, install/uninstall, VDD install
  display_ccd.rs                    Windows-only: CCD API for VDD primary topology
  input_injector.rs                 enigo: mouse/keyboard/scroll + paste type_text +
                                    modifier release
  input_uinput.rs                   Linux-only: virtual /dev/uinput keyboard
                                    (bypasses GDM 42 XKB scramble, works on Wayland)
  file_transfer.rs                  Server-side file transfer handler

  audio/                          Cross-platform audio capture → Opus
    mod.rs                            dispatch + AudioChunk + AudioDropCounter
    pulse.rs                          PulseAudio monitor (Linux)
    wasapi.rs                         WASAPI loopback (Windows)

  capture/                        Screen capture backends
    mod.rs                            cfg gates
    scrap.rs                          ScrapCapture (cross-platform CPU capture)
    gdi.rs                            GDI BitBlt fallback for lock screen / Session 0
    pipewire.rs                       PipeWire + XDG Desktop Portal (Wayland, feature
                                      `wayland`)

  encode/                         CPU encoders (GPU encoders live in phantom-gpu)
    mod.rs                            re-exports
    h264.rs                           OpenH264Encoder (impl FrameEncoder)

  transport/                      All implement MessageSender / MessageReceiver
    mod.rs                            re-exports
    tcp.rs                            TCP: Plain or ChaCha20-Poly1305, split via try_clone
    quic.rs                           QUIC: quinn, self-signed TLS, keep-alive
    ws.rs                             WebServerTransport: HTTPS static (serves WASM)
                                      + WSS upgrade + WebRTC POST /rtc + JWT auth
    webrtc.rs                         Phantom-owned WebRTC run loop + session bridge
                                      (media tracks + input/control DC, feature `webrtc`)
    webrtc/backend_phantom.rs         ICE/STUN + DTLS + SRTP/SRTCP + RTP packetization
                                      + DataChannel wiring
    webrtc/sctp.rs                    DataChannel adapter over in-tree `phantom-sctp`

  bin/mock_server.rs              Animated H.264 frames without screen capture
                                  (for transport + codec pipeline tests)

crates/client/src/                Native client (winit + softbuffer)
  main.rs                           winit ApplicationHandler, reconnect loop, file
                                    transfer, clipboard sync, borderless fullscreen,
                                    macOS transparent title bar, ClientHello with
                                    preferred_server_resolution
  display_winit.rs                  softbuffer rendering, coordinate mapping, cursor
                                    overlay
  input_capture.rs                  winit → phantom KeyCode, Sunshine-style scroll
                                    accumulation
  decode_h264.rs                    OpenH264Decoder (CPU H.264 fallback)
  decode_av1.rs                    Dav1dDecoder (AV1 software decode; feature `av1`)
  decode_videotoolbox.rs            VideoToolbox hardware H.264 (macOS)
  audio_playback.rs                 Opus decode → cpal ring buffer (300ms target,
                                    100ms prime, underrun/trim metrics)
  file_transfer.rs                  Client-side file transfer handler
  transport_tcp.rs                  TCP client
  transport_quic.rs                 QUIC client

crates/web/src/                   Browser client (Rust → WASM)
  lib.rs                            WASM entry, setup_webrtc (POST /rtc) + setup_ws
                                    fallback, media-track receive path (`<video>`/`<audio>`)
                                    + DataChannel control/input, WebCodecs H.264/AV1
                                    decode for WSS path, mouse/keyboard/scroll input,
                                    JWT passthrough, stuck-key prevention, ClientHello
                                    with preferred_viewport
  pkg/                              wasm-pack output (committed so server can build
                                    without running wasm-pack every time)

crates/phantom-sctp/src/          Local SCTP crate used by WebRTC DataChannels
  lib.rs                            Endpoint/association glue and stream I/O

crates/server/web/
  index.html                        Minimal HTML loader (embedded via include_str!
                                    in transport/ws.rs)

crates/gpu/src/                   NVIDIA GPU pipeline (runtime dlopen, no build dep)
  lib.rs                            Module exports
  dl.rs                             Runtime dlopen/dlsym abstraction
  sys.rs                            C FFI: CUDA, NVENC (SDK 12.1), NVFBC (v1.8/1.9)
  cuda.rs                           CUDA driver API: context, memory, primary ctx
  nvenc.rs                          NvencEncoder: H.264 + AV1 GPU encode (uses
                                    phantom_core::color for BGRA→NV12)
  nvdec.rs                          NvdecDecoder: NVDEC hardware decode H.264 + AV1
                                    via CUVID API (feature `nvdec`)
  nvfbc.rs                          NvfbcCapture: GPU screen capture via NVFBC
                                    (Linux only)
  probe.rs                          GPU capability probe: NVENC codecs, AV1 detection
  dxgi.rs                           DxgiCapture: Desktop Duplication → D3D11 texture
                                    (Windows only)
  dxgi_nvenc.rs                     DxgiNvencPipeline: DXGI + NVENC zero-copy fused
                                    struct (Windows only); wrapped by
                                    DxgiNvencPipelineAdapter in server/pipeline.rs

crates/bench/src/main.rs          Encoder benchmark: OpenH264 vs NVENC × resolutions

Tests (136 total, run in CI)
----------------------------
crates/server/tests/
  e2e_headless.rs                   End-to-end without GUI capture
  multi_transport_test.rs           TCP + WSS + QUIC simultaneously
  pipeline_test.rs                  H.264 encode/decode round trip, protocol round trip
  quic_test.rs                      QUIC ALPN + handshake
  sctp_backpressure_test.rs         chunk-framing compatibility tests
  wan_test.rs                       TCP proxy with delay/jitter, 8 WAN scenarios
  session_loop_test.rs              Mock capture/encoder/transport, 7 session-loop
                                    behavior tests (safety net for task #23)
crates/server/src/session.rs      CongestionTracker / AdaptiveBitrate /
                                    classify_session_error unit tests
crates/server/src/doorbell.rs     ghost-set policy unit tests
crates/core/src/protocol.rs       Message round-trips
crates/core/src/color.rs          SIMD vs scalar correctness

Top-level
---------
Cargo.toml                        Workspace
.github/workflows/                ci.yml (fmt + clippy + test on Linux + Windows;
                                  Doc; WASM build) + release.yml (multi-platform
                                  artifacts on tag)
Dockerfile + docker-entrypoint.sh XFCE desktop test environment
README.md                         Public-facing
CLAUDE.md                         AI-assistant guide, points at docs/
docs/                             Reference documentation (this directory)
install.sh / install.ps1          One-line installers (fetch from GitHub Releases
                                  by default; env overrides support local
                                  installer iteration; install.sh --autologin
                                  configures Linux VM auto-login + watchdog)
LICENSE                           MIT
```
