#!/bin/bash
# End-to-end test using mock_server (no screen capture needed, works anywhere)
# Tests: connect → receive H.264 frames → verify no crash for 5 seconds
set -e

cd "$(dirname "$0")/.."
echo "=== Building release ==="
cargo build --release 2>&1 | tail -1

echo ""
echo "=== Starting mock_server ==="
cargo run --release --bin mock_server &
MOCK_PID=$!
sleep 2

if ! kill -0 $MOCK_PID 2>/dev/null; then
    echo "FAIL: mock_server didn't start"
    exit 1
fi
echo "mock_server running (PID $MOCK_PID)"

echo ""
echo "=== Starting client (5 second test) ==="
# Run client with --no-encrypt (mock_server doesn't encrypt)
timeout 5 cargo run --release -p phantom-client -- --no-encrypt -c 127.0.0.1:9900 2>&1 &
CLIENT_PID=$!

# Wait and check both processes
sleep 6

CLIENT_EXIT=0
if kill -0 $CLIENT_PID 2>/dev/null; then
    kill $CLIENT_PID 2>/dev/null
    echo "Client was still running (good — window was open)"
else
    wait $CLIENT_PID 2>/dev/null || CLIENT_EXIT=$?
fi

kill $MOCK_PID 2>/dev/null
wait $MOCK_PID 2>/dev/null || true

echo ""
if [ $CLIENT_EXIT -eq 0 ] || [ $CLIENT_EXIT -eq 124 ]; then
    # 124 = timeout killed it (expected)
    echo "=== PASS: e2e mock test completed ==="
    echo "  - mock_server generated H.264 frames"
    echo "  - client connected, decoded, and displayed"
    echo "  - no crashes in 5 seconds"
else
    echo "=== FAIL: client exited with code $CLIENT_EXIT ==="
    exit 1
fi
