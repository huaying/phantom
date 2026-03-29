#!/bin/bash
set -e

MODE=${1:-server}
shift 2>/dev/null || true

echo "=== Phantom Docker ==="
echo "Mode: $MODE"
echo "Resolution: $RESOLUTION"
echo "Display: $DISPLAY"

# Start Xvfb (virtual X11 framebuffer)
Xvfb $DISPLAY -screen 0 $RESOLUTION +extension RANDR &
sleep 1

# Launch desktop or basic X11 apps
if [ "$MODE" != "mock" ]; then
    if command -v startxfce4 &>/dev/null; then
        # Full XFCE desktop
        export XDG_SESSION_TYPE=x11
        dbus-launch startxfce4 &
        sleep 3
        # Remove XFCE Super key shortcuts (conflict with macOS Cmd key)
        xfconf-query -c xfce4-keyboard-shortcuts -p "/commands/custom/<Super>e" -r 2>/dev/null
        xfconf-query -c xfce4-keyboard-shortcuts -p "/commands/custom/<Super>p" -r 2>/dev/null
        xfconf-query -c xfce4-keyboard-shortcuts -p "/commands/custom/<Super>r" -r 2>/dev/null
    else
        # Fallback
        xclock -geometry 200x200+10+10 &
        xeyes -geometry 150x100+250+10 &
        xterm -geometry 80x24+10+250 &
        sleep 1
    fi
fi

echo "=== X11 ready ==="

case "$MODE" in
    server)
        echo "Starting phantom-server..."
        exec phantom-server --no-encrypt "$@"
        ;;
    server-web)
        echo "Starting phantom-server (web)..."
        echo "Open http://localhost:9900 in your browser"
        exec phantom-server --transport web --no-encrypt "$@"
        ;;
    server-encrypted)
        echo "Starting phantom-server (encrypted)..."
        exec phantom-server "$@"
        ;;
    server-quic)
        echo "Starting phantom-server (QUIC)..."
        exec phantom-server --transport quic --no-encrypt "$@"
        ;;
    mock)
        echo "Starting mock-server..."
        exec mock_server
        ;;
    *)
        echo "Unknown mode: $MODE"
        echo "Usage: docker run phantom [server|server-encrypted|server-quic|mock]"
        exit 1
        ;;
esac
