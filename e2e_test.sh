#!/bin/bash
# E2E test suite for Phantom — v2
cd /home/horde/phantom
BIN=./target/release

PASS=0; FAIL=0; SKIP=0
pass() { echo "  ✅ PASS: $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ FAIL: $1 — $2"; FAIL=$((FAIL+1)); }
skip() { echo "  ⏭️  SKIP: $1 — $2"; SKIP=$((SKIP+1)); }

cleanup() {
    [ -n "$SERVER_PID" ] && kill $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null; SERVER_PID=""
}

echo "═══════════════════════════════════════════"
echo "  Phantom E2E Test Suite"
echo "═══════════════════════════════════════════"
echo ""

# ── 1: --list-displays ──
echo "▶ Test 1: --list-displays"
OUT=$($BIN/phantom-server --list-displays 2>&1) || true
echo "$OUT" | grep -q "Display 0:" && pass "--list-displays" || fail "--list-displays" "$OUT"

# ── 2: Invalid display ──
echo "▶ Test 2: Invalid display index"
OUT=$($BIN/phantom-server --display 99 --transport tcp --no-encrypt --listen 127.0.0.1:19910 2>&1) || true
echo "$OUT" | grep -q "out of range" && pass "invalid display rejected" || fail "invalid display" "$OUT"

# ── 3: TCP unencrypted ──
echo "▶ Test 3: TCP unencrypted E2E"
$BIN/phantom-server --transport tcp --no-encrypt --listen 127.0.0.1:19911 >/tmp/e2e-3.log 2>&1 &
SERVER_PID=$!; sleep 2
CLIENT_OUT=$(timeout 6 $BIN/phantom-client --no-encrypt -c 127.0.0.1:19911 2>&1) || true
sleep 1; SLOG=$(cat /tmp/e2e-3.log)
cleanup

echo "$CLIENT_OUT" | grep -q "connected.*1920.*1080" && pass "TCP: client connected" || fail "TCP: connect" "$(echo "$CLIENT_OUT" | head -2)"
echo "$CLIENT_OUT" | grep -q "video_fps" && pass "TCP: video streaming" || fail "TCP: video" "no fps"
echo "$SLOG" | grep -q "session started" && pass "TCP: server session" || fail "TCP: session" "$(echo "$SLOG" | tail -5)"
echo "$SLOG" | grep -q "first keyframe sent" && pass "TCP: keyframe" || fail "TCP: keyframe" "none in log"

# ── 4: TCP encrypted ──
echo "▶ Test 4: TCP encrypted E2E"
$BIN/phantom-server --transport tcp --listen 127.0.0.1:19912 >/tmp/e2e-4.log 2>&1 &
SERVER_PID=$!; sleep 2
KEY=$(grep -oP '(?<=--key )\S+' /tmp/e2e-4.log | head -1)
if [ -z "$KEY" ]; then
    fail "encrypted: key gen" "no key in log"; cleanup
else
    CLIENT_OUT=$(timeout 6 $BIN/phantom-client --key "$KEY" -c 127.0.0.1:19912 2>&1) || true
    cleanup
    echo "$CLIENT_OUT" | grep -q "connected" && pass "encrypted: connected" || fail "encrypted: connect" "$(echo "$CLIENT_OUT" | head -2)"
    echo "$CLIENT_OUT" | grep -q "video_fps" && pass "encrypted: video" || fail "encrypted: video" "no fps"
fi

# ── 5: Wrong key ──
echo "▶ Test 5: Wrong encryption key"
$BIN/phantom-server --transport tcp --listen 127.0.0.1:19913 >/tmp/e2e-5.log 2>&1 &
SERVER_PID=$!; sleep 2
WRONG="0000000000000000000000000000000000000000000000000000000000000000"
CLIENT_OUT=$(timeout 4 $BIN/phantom-client --key "$WRONG" -c 127.0.0.1:19913 2>&1) || true
cleanup
if ! echo "$CLIENT_OUT" | grep -q "connected.*1920"; then
    pass "wrong key: connection rejected"
else
    fail "wrong key" "client connected with wrong key!"
fi

# ── 6: WebSocket ──
echo "▶ Test 6: WebSocket transport"
$BIN/phantom-server --transport web --no-encrypt --listen 127.0.0.1:19914 >/tmp/e2e-6.log 2>&1 &
SERVER_PID=$!; sleep 2
SLOG=$(cat /tmp/e2e-6.log)
echo "$SLOG" | grep -q "https://" && pass "WS: HTTPS listener" || fail "WS: listener" "$(echo "$SLOG" | tail -3)"

CURL_OUT=$(curl -sk https://127.0.0.1:19914/ 2>&1 | head -c 200)
[ -n "$CURL_OUT" ] && pass "WS: HTTPS serves content" || fail "WS: content" "empty response"
cleanup

# ── 7: Multi-transport ──
echo "▶ Test 7: Multi-transport (tcp,web)"
$BIN/phantom-server --transport tcp,web --no-encrypt --listen 127.0.0.1:19915 >/tmp/e2e-7.log 2>&1 &
SERVER_PID=$!; sleep 2
SLOG=$(cat /tmp/e2e-7.log)
echo "$SLOG" | grep -q "TCP server listening" && pass "multi: TCP listener" || fail "multi: TCP" "not found"
echo "$SLOG" | grep -q "https://" && pass "multi: Web listener" || fail "multi: Web" "not found"

CLIENT_OUT=$(timeout 5 $BIN/phantom-client --no-encrypt -c 127.0.0.1:19915 2>&1) || true
echo "$CLIENT_OUT" | grep -q "connected" && pass "multi: TCP client works" || fail "multi: TCP client" "no connect"
cleanup

# ── 8: Session replacement ──
echo "▶ Test 8: Session replacement"
$BIN/phantom-server --transport tcp --no-encrypt --listen 127.0.0.1:19916 >/tmp/e2e-8.log 2>&1 &
SERVER_PID=$!; sleep 2

timeout 4 $BIN/phantom-client --no-encrypt -c 127.0.0.1:19916 >/dev/null 2>&1 &
C1=$!; sleep 2

C2_OUT=$(timeout 4 $BIN/phantom-client --no-encrypt -c 127.0.0.1:19916 2>&1) || true
wait $C1 2>/dev/null || true
sleep 1; SLOG=$(cat /tmp/e2e-8.log)
cleanup

echo "$C2_OUT" | grep -q "connected" && pass "replace: 2nd client" || fail "replace: 2nd client" "no connect"
SC=$(echo "$SLOG" | grep -c "session started")
[ "$SC" -ge 2 ] && pass "replace: $SC sessions" || fail "replace" "$SC session(s), expected ≥2"

# ── 9: Graceful shutdown ──
echo "▶ Test 9: Graceful shutdown"
$BIN/phantom-server --transport tcp --no-encrypt --listen 127.0.0.1:19917 >/tmp/e2e-9.log 2>&1 &
SERVER_PID=$!; sleep 2

kill -INT $SERVER_PID 2>/dev/null
for i in $(seq 1 40); do kill -0 $SERVER_PID 2>/dev/null || break; sleep 0.2; done

if ! kill -0 $SERVER_PID 2>/dev/null; then
    pass "shutdown: exited after SIGINT"
else
    fail "shutdown" "still running"; kill -9 $SERVER_PID 2>/dev/null
fi
SLOG=$(cat /tmp/e2e-9.log)
echo "$SLOG" | grep -q "shutdown signal\|goodbye" && pass "shutdown: clean exit logged" || fail "shutdown: log" "$(echo "$SLOG" | tail -3)"
SERVER_PID=""

# ── 10: Display logged ──
echo "▶ Test 10: Display selection logged"
$BIN/phantom-server --display 0 --transport tcp --no-encrypt --listen 127.0.0.1:19918 >/tmp/e2e-10.log 2>&1 &
SERVER_PID=$!; sleep 3
SLOG=$(cat /tmp/e2e-10.log)
cleanup
echo "$SLOG" | sed 's/\x1b\[[0-9;]*m//g' | grep -q "display=0" && pass "display=0 logged" || fail "display log" "not found"

# ── Summary ──
echo ""
echo "═══════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "═══════════════════════════════════════════"
rm -f /tmp/e2e-*.log
[ $FAIL -eq 0 ] && exit 0 || exit 1
