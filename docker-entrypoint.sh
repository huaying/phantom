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

# Launch X11 apps for visual content
if [ "$MODE" != "mock" ]; then
    xclock -geometry 200x200+10+10 &
    xeyes -geometry 150x100+250+10 &
    xterm -geometry 80x24+10+250 -e "echo '=== Phantom Remote Desktop ==='; echo 'Linux $(uname -r)'; echo 'Resolution: $RESOLUTION'; echo ''; echo 'Try: move mouse (xeyes follows), type here'; echo ''; exec bash" &
    sleep 1
fi

echo "=== X11 ready ==="

case "$MODE" in
    server)
        echo "Starting phantom-server..."
        exec phantom-server --no-encrypt "$@"
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
