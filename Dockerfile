FROM rust:1.94-bookworm AS builder

# Install build deps for openh264 (needs nasm) and scrap (needs X11 dev libs)
RUN apt-get update && apt-get install -y --no-install-recommends \
    nasm \
    libx11-dev \
    libxext-dev \
    libxrandr-dev \
    libxtst-dev \
    libxdo-dev \
    libxcb-randr0-dev \
    libxcb-shm0-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release -p phantom-server --bin phantom-server --bin mock_server

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    xvfb \
    x11-apps \
    xterm \
    libx11-6 \
    libxext6 \
    libxrandr2 \
    libxtst6 \
    libxdo3 \
    libxcb-randr0 \
    libxcb-shm0 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/phantom-server /usr/local/bin/
COPY --from=builder /build/target/release/mock_server /usr/local/bin/

# Startup script
COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 9900/tcp 9900/udp

ENV DISPLAY=:99
ENV RESOLUTION=1280x720x24

ENTRYPOINT ["docker-entrypoint.sh"]
