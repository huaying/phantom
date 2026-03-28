#!/bin/bash
set -e

MODE=${1:-server}

echo "=== Phantom Docker ==="
echo "Mode: $MODE"
echo "Resolution: $RESOLUTION"
echo "Display: $DISPLAY"

# Start Xvfb (virtual X11 framebuffer)
Xvfb $DISPLAY -screen 0 $RESOLUTION +extension RANDR &
sleep 1

# Launch some X11 apps so there's something to see
if [ "$MODE" != "mock" ]; then
    # xclock in top-left
    xclock -geometry 200x200+10+10 &
    # xeyes follows cursor
    xeyes -geometry 150x100+250+10 &
    # xterm with system info
    xterm -geometry 80x20+10+250 -e "echo 'Phantom Server running on Linux'; echo 'Resolution: $RESOLUTION'; echo 'PID: $$'; uname -a; echo ''; echo 'This is a live X11 desktop.'; echo 'Move your mouse and type!'; bash" &
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
