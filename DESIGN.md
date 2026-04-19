# Phantom — Design

A high-performance, open-source remote desktop built in Rust. Target:
Parsec-class latency, single binary deployment, browser + native access.

The detailed design — encoding pipeline, transport choices, GPU zero-copy
paths, the rationale for every architectural decision — has moved to:

- **[docs/architecture.md](docs/architecture.md)** — system overview, why
  WebRTC DataChannel / WSS / QUIC, key implementation details
- **[docs/features.md](docs/features.md)** — current capability list
- **[docs/file-map.md](docs/file-map.md)** — module layout
- **[docs/pitfalls.md](docs/pitfalls.md)** — bugs we shipped and the fixes

For day-to-day usage, see the [README](README.md).

## License

MIT
