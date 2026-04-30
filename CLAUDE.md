# Phantom — AI assistant guide

You're working on Phantom, a Rust remote-desktop server. This file is the
short index of must-knows for AI assistants. Everything detailed lives in
`docs/`:

- **[docs/architecture.md](docs/architecture.md)** — what the system is,
  why we picked WebRTC DataChannel / WSS / QUIC, key implementation
  details (str0m run_loop, Windows service mode, GPU pipeline).
- **[docs/pitfalls.md](docs/pitfalls.md)** — bugs we've shipped and had to
  track down. Re-read before touching the relevant area.
- **[docs/features.md](docs/features.md)** — current capability list +
  full CLI reference.
- **[docs/file-map.md](docs/file-map.md)** — module layout per crate.

## Build commands

```bash
# WASM web client must build BEFORE the server (server embeds the bundle
# via include_bytes! — stale WASM = stale browser bundle).
wasm-pack build crates/web --target web --no-typescript

cargo build --release                                      # workspace
cargo build --release --features webrtc                    # +WebRTC DataChannel
cargo build --release -p phantom-server                    # server only
cargo test --workspace                                     # 136 tests
cargo clippy --workspace -- -D warnings                    # zero warnings

# GPU benchmarks (NVIDIA + DISPLAY=...)
DISPLAY=:0 cargo run --release -p phantom-bench
DISPLAY=:0 cargo run --release --example nvenc_bench -p phantom-gpu
```

**WASM build order**: rebuild WASM whenever you change anything under
`crates/web/`, or the server will ship stale bytes.

**Default feature set**: `phantom-server` defaults are
`["web-client", "audio", "sso", "webrtc"]`. `--no-default-features` drops
`web-client` and the browser receives a stub that prints `console.error` and stays black.
Don't combine `--no-default-features` with anything that needs the web
client.

## Test environment

```bash
# Docker (CPU-only):
docker build -t phantom .
docker run --rm -p 9900:9900 -p 9901:9901 -p 9902:9902/udp \
    -e PHANTOM_HOST=127.0.0.1 phantom server-web
# → open http://127.0.0.1:9900

# Native client:
docker run --rm -p 9900:9900 phantom server
cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900

# GPU test VM (A40, driver 550, Ubuntu 24.04):
ssh horde@10.57.233.13
DISPLAY=:0 cargo run --release -p phantom-bench
DISPLAY=:0 phantom-server --encoder nvenc --capture nvfbc --transport web
```

## Production logging (0.4.0)

```bash
phantom-server --log-file /var/log/phantom/phantom.log \
               --log-rotate daily --log-keep 7
```

Stats line includes `session_id`, `jitter`, `audio_drops_5s`. Session
end is logged with structured `reason=peer closed | cancelled |
network error: ...`.

## When in doubt

Read [docs/pitfalls.md](docs/pitfalls.md) first if you're touching:
capture/encode (CPU or GPU), transports, decoders, session lifecycle,
WebRTC, NVFBC/NVENC, DXGI, Windows service mode, or input forwarding.
The list is bug-by-bug, with the exact failure mode and fix.
