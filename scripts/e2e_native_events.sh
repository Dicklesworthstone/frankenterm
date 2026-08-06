#!/usr/bin/env bash
# e2e_native_events.sh — End-to-end validation of the native event bridge.
#
# Tests authenticated native-bridge connection and disconnect lifecycle.
# Raw pane-output bytes are not emitted by the current GUI bridge; polling
# remains the authoritative text-capture path.
#
# Prerequisites:
#   - frankenterm (CLI binary with ft watch subcommand) built and on PATH
#   - frankenterm-gui built and on PATH (or use FRANKENTERM_GUI env var)
#   - A graphical session in which launching the GUI is acceptable
#
# This test opens a real GUI window and may take focus. It is deliberately
# guarded against accidental/agent execution.
# Usage: FRANKENTERM_ALLOW_GUI_E2E=1 ./scripts/e2e_native_events.sh
#
# Exit codes:
#   0 = all checks passed
#   1 = one or more checks failed
#   2 = disruptive GUI launch was not explicitly authorized

set -euo pipefail

if [ "${FRANKENTERM_ALLOW_GUI_E2E:-}" != "1" ]; then
    echo "refusing to launch FrankenTerm GUI without FRANKENTERM_ALLOW_GUI_E2E=1" >&2
    exit 2
fi

FT_GUI="${FRANKENTERM_GUI:-frankenterm-gui}"
FT_CLI="${FRANKENTERM_CLI:-frankenterm}"
LOG_DIR=$(mktemp -d /tmp/e2e-native-events.XXXXXX)
WORKSPACE_DIR="$LOG_DIR/workspace"
NATIVE_RUNTIME_DIR="$WORKSPACE_DIR/native-runtime"
SOCKET_PATH="$NATIVE_RUNTIME_DIR/events.sock"
CONFIG_PATH="$WORKSPACE_DIR/ft.toml"
PASS=0
FAIL=0

case "$(uname -s)" in
    Darwin|Linux|FreeBSD|DragonFly) ;;
    *)
        echo "native event E2E requires an authenticated Unix peer-credential target" >&2
        exit 1
        ;;
esac

# Preserve relative executable paths before entering the isolated workspace.
START_DIR=$(pwd -P)
case "$FT_GUI" in
    /*) ;;
    */*) FT_GUI="$START_DIR/$FT_GUI" ;;
esac
case "$FT_CLI" in
    /*) ;;
    */*) FT_CLI="$START_DIR/$FT_CLI" ;;
esac

mkdir -m 700 "$WORKSPACE_DIR" "$NATIVE_RUNTIME_DIR"
{
    echo '[native]'
    echo 'enabled = true'
    printf 'socket_path = "%s"\n' "$SOCKET_PATH"
} >"$CONFIG_PATH"

# Invoked through the EXIT trap below.
# shellcheck disable=SC2329
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
echo "Config: $CONFIG_PATH"
echo "Log dir: $LOG_DIR"
echo ""

# Step 1: Start ft watch in foreground mode with an isolated, explicitly
# enabled native-event configuration. The listener exclusively owns
# authenticated, identity-pinned stale-socket cleanup; this harness must never
# unlink a caller-selected path.
echo "[step 1] Starting ft watch..."
(
    cd "$WORKSPACE_DIR"
    exec env RUST_LOG=info WEZTERM_FT_SOCKET="$SOCKET_PATH" \
        "$FT_CLI" --config "$CONFIG_PATH" --workspace "$WORKSPACE_DIR" watch --foreground
) >"$LOG_DIR/watch-stdout.log" 2>"$LOG_DIR/watch-stderr.log" &
WATCH_PID=$!
sleep 2

if kill -0 "$WATCH_PID" 2>/dev/null; then
    check "ft watch started" "pass"
else
    check "ft watch started" "fail"
    echo "ft watch failed to start. Check $LOG_DIR/watch-stderr.log"
    exit 1
fi

# Step 2: Start frankenterm-gui from the same isolated directory. Its ft-core
# configuration loader reads ./ft.toml; the explicit environment value is the
# same path and cannot bypass disablement because the config enables native
# events first.
echo "[step 2] Starting frankenterm-gui..."
(
    cd "$WORKSPACE_DIR"
    exec env RUST_LOG=info WEZTERM_FT_SOCKET="$SOCKET_PATH" "$FT_GUI"
) >"$LOG_DIR/gui-stdout.log" 2>"$LOG_DIR/gui-stderr.log" &
GUI_PID=$!
sleep 3

if kill -0 "$GUI_PID" 2>/dev/null; then
    check "frankenterm-gui started" "pass"
else
    check "frankenterm-gui started" "fail"
    echo "GUI failed to start. Check $LOG_DIR/gui-stderr.log"
    exit 1
fi

# Step 3: Check that the authenticated native event bridge connected.
if grep -q "Native event bridge: socket found" "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
elif grep -q "native_bridge" "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
else
    check "GUI connected to native event socket" "fail"
fi

# Step 4: Check ft watch bound the explicitly enabled listener.
if grep -q "Starting explicitly enabled native event listener\|Native event listener bound" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
    check "ft watch bound explicitly enabled native listener" "pass"
else
    check "ft watch bound explicitly enabled native listener" "fail"
fi

# Step 5: Stop the harness-owned GUI and verify ft watch stays alive.
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
