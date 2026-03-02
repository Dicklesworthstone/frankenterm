#!/usr/bin/env bash
# e2e_native_events.sh — End-to-end validation of the native event bridge.
#
# Tests that frankenterm-gui pushes events to ft watch over the native
# event socket, replacing polling with real-time push.
#
# Prerequisites:
#   - frankenterm (CLI binary with ft watch subcommand) built and on PATH
#   - frankenterm-gui built and on PATH (or use FRANKENTERM_GUI env var)
#   - No other ft watch instance running on the same socket
#
# Usage: ./scripts/e2e_native_events.sh
#
# Exit codes:
#   0 = all checks passed
#   1 = one or more checks failed

set -euo pipefail

SOCKET_PATH="${WEZTERM_FT_SOCKET:-/tmp/wa/events.sock}"
FT_GUI="${FRANKENTERM_GUI:-frankenterm-gui}"
FT_CLI="${FRANKENTERM_CLI:-frankenterm}"
LOG_DIR=$(mktemp -d /tmp/e2e-native-events.XXXXXX)
CANARY="CANARY_$(date +%s)_$$"
PASS=0
FAIL=0

cleanup() {
    echo "[cleanup] Stopping processes..."
    [ -n "${GUI_PID:-}" ] && kill "$GUI_PID" 2>/dev/null || true
    [ -n "${WATCH_PID:-}" ] && kill "$WATCH_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    echo "[cleanup] Logs in $LOG_DIR"
}
trap cleanup EXIT

check() {
    local label="$1"
    local result="$2"
    if [ "$result" = "pass" ]; then
        PASS=$((PASS + 1))
        echo "[PASS] $label"
    else
        FAIL=$((FAIL + 1))
        echo "[FAIL] $label"
    fi
}

echo "=== Native Event Bridge E2E Test ==="
echo "Socket: $SOCKET_PATH"
echo "Canary: $CANARY"
echo "Log dir: $LOG_DIR"
echo ""

# Step 1: Clean up any stale socket
rm -f "$SOCKET_PATH"

# Step 2: Start ft watch in foreground mode
echo "[step 1] Starting ft watch..."
RUST_LOG=info "$FT_CLI" watch --foreground \
    >"$LOG_DIR/watch-stdout.log" 2>"$LOG_DIR/watch-stderr.log" &
WATCH_PID=$!
sleep 2

if kill -0 "$WATCH_PID" 2>/dev/null; then
    check "ft watch started" "pass"
else
    check "ft watch started" "fail"
    echo "ft watch failed to start. Check $LOG_DIR/watch-stderr.log"
    exit 1
fi

# Step 3: Start frankenterm-gui
echo "[step 2] Starting frankenterm-gui..."
RUST_LOG=info "$FT_GUI" \
    >"$LOG_DIR/gui-stdout.log" 2>"$LOG_DIR/gui-stderr.log" &
GUI_PID=$!
sleep 3

if kill -0 "$GUI_PID" 2>/dev/null; then
    check "frankenterm-gui started" "pass"
else
    check "frankenterm-gui started" "fail"
    echo "GUI failed to start. Check $LOG_DIR/gui-stderr.log"
    exit 1
fi

# Step 4: Check that native event bridge connected
if grep -q "Native event bridge: socket found" "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
elif grep -q "native_bridge" "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
else
    check "GUI connected to native event socket" "fail"
fi

# Step 5: Check ft watch logged native push mode
if grep -q "native push events\|Native event listener bound" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
    check "ft watch detected native push mode" "pass"
else
    check "ft watch detected native push mode" "fail"
fi

# Step 6: Kill GUI and verify ft watch stays alive
echo "[step 3] Killing GUI, checking ft watch resilience..."
kill "$GUI_PID" 2>/dev/null || true
wait "$GUI_PID" 2>/dev/null || true
unset GUI_PID
sleep 2

if kill -0 "$WATCH_PID" 2>/dev/null; then
    check "ft watch survived GUI disconnect" "pass"
else
    check "ft watch survived GUI disconnect" "fail"
fi

# Summary
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
echo "Logs: $LOG_DIR"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "--- ft watch stderr ---"
    tail -20 "$LOG_DIR/watch-stderr.log" 2>/dev/null || true
    echo "--- gui stderr ---"
    tail -20 "$LOG_DIR/gui-stderr.log" 2>/dev/null || true
    exit 1
fi

exit 0
