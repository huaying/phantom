#!/bin/sh
# Phantom Remote Desktop — install script
# Usage: curl -fsSL https://raw.githubusercontent.com/huaying/phantom/main/install.sh | sh
#
# Installs phantom-server and/or phantom-client to /usr/local/bin.
# On Linux, also installs required runtime libraries.

set -e

REPO="huaying/phantom"
INSTALL_DIR="/usr/local/bin"

# --- Detect OS and Arch ---
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)  OS="linux" ;;
    darwin) OS="macos" ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Detected: ${OS}/${ARCH}"

# --- Determine what to install ---
INSTALL_SERVER=false
INSTALL_CLIENT=false

if [ "$1" = "server" ]; then
    INSTALL_SERVER=true
elif [ "$1" = "client" ]; then
    INSTALL_CLIENT=true
elif [ "$1" = "both" ]; then
    INSTALL_SERVER=true
    INSTALL_CLIENT=true
else
    # Default: server on Linux, client on macOS
    case "$OS" in
        linux) INSTALL_SERVER=true ;;
        macos) INSTALL_CLIENT=true ;;
    esac
fi

# --- Install Linux runtime dependencies ---
if [ "$OS" = "linux" ]; then
    echo "Installing runtime dependencies..."

    if command -v apt-get > /dev/null 2>&1; then
        # Debian / Ubuntu
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb1 libxcb-shm0 libxcb-randr0 libxtst6 libxdo3 libpulse0"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            # Client: winit needs xcb libs + softbuffer renders via xcb,
            # alsa for audio output.
            PKGS="$PKGS libxcb1 libxcb-shm0 libasound2"
        fi
        if [ -n "$PKGS" ]; then
            sudo apt-get update -qq
            sudo apt-get install -y --no-install-recommends $PKGS || true
        fi

    elif command -v dnf > /dev/null 2>&1; then
        # Fedora / RHEL
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb libxdo libXtst pulseaudio-libs"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            PKGS="$PKGS libxcb alsa-lib"
        fi
        if [ -n "$PKGS" ]; then
            sudo dnf install -y $PKGS || true
        fi

    elif command -v pacman > /dev/null 2>&1; then
        # Arch Linux
        PKGS=""
        if [ "$INSTALL_SERVER" = true ]; then
            PKGS="libxcb xdotool libxtst libpulse"
        fi
        if [ "$INSTALL_CLIENT" = true ]; then
            PKGS="$PKGS libxcb alsa-lib"
        fi
        if [ -n "$PKGS" ]; then
            sudo pacman -S --needed --noconfirm $PKGS || true
        fi

    else
        echo "Warning: could not detect package manager. You may need to install runtime libraries manually."
        echo "  Server: libxcb, libxdo, libpulse"
        echo "  Client: libasound (ALSA)"
    fi
fi

# --- Get latest release URL ---
BASE_URL="https://github.com/${REPO}/releases/latest/download"

download_and_install() {
    name="$1"
    url="${BASE_URL}/${name}-${OS}-${ARCH}"

    echo "Downloading ${name}..."
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL "$url" -o "/tmp/${name}"
    elif command -v wget > /dev/null 2>&1; then
        wget -qO "/tmp/${name}" "$url"
    else
        echo "Error: curl or wget required"; exit 1
    fi

    chmod +x "/tmp/${name}"

    # Install — use sudo if needed
    if [ -w "$INSTALL_DIR" ]; then
        mv "/tmp/${name}" "${INSTALL_DIR}/${name}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "/tmp/${name}" "${INSTALL_DIR}/${name}"
    fi

    echo "Installed: ${INSTALL_DIR}/${name}"
}

# --- Install ---
if [ "$INSTALL_SERVER" = true ]; then
    download_and_install "phantom-server"
fi

if [ "$INSTALL_CLIENT" = true ]; then
    download_and_install "phantom-client"
fi

# --- Post-install hints ---
echo ""
echo "Done!"
if [ "$INSTALL_SERVER" = true ]; then
    echo ""
    echo "Start server:"
    echo "  phantom-server"
    echo "  # TCP:9900 (native client) + Web:9901 (browser: https://localhost:9901)"
    echo ""
    echo "With GPU (NVIDIA):"
    echo "  DISPLAY=:0 phantom-server --capture nvfbc --encoder nvenc"
fi
if [ "$INSTALL_CLIENT" = true ]; then
    echo ""
    echo "Connect to server:"
    echo "  phantom-client -c <server-ip>:9900"
fi
