#!/usr/bin/env bash
# e2e_native_events.sh — End-to-end validation of the native event bridge.
#
# Tests authenticated native-bridge connection and disconnect lifecycle.
# Raw pane-output bytes are not emitted by the current GUI bridge; polling
# remains the authoritative text-capture path.
#
# Prerequisites:
#   - Explicit absolute candidate paths in FRANKENTERM_CLI/FRANKENTERM_GUI
#   - Their common candidate root in FRANKENTERM_CANDIDATE_ROOT
#   - Full source SHA/profile metadata for the retained artifact manifest
#   - A graphical session in which launching the GUI is acceptable
#
# This test opens a real GUI window and may take focus. It is deliberately
# guarded against accidental/agent execution.
# Usage requires BOTH disruptive-interaction acknowledgements; see the checks
# below. Automation must never manufacture those acknowledgements.
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
if [ "${FRANKENTERM_GUI_E2E_FOCUS_ACK:-}" != "I_ACCEPT_FOCUS_DISRUPTION" ]; then
    echo "refusing GUI launch without FRANKENTERM_GUI_E2E_FOCUS_ACK=I_ACCEPT_FOCUS_DISRUPTION" >&2
    exit 2
fi

if [ -z "${FRANKENTERM_GUI:-}" ] || [ -z "${FRANKENTERM_CLI:-}" ] || \
   [ -z "${FRANKENTERM_CANDIDATE_ROOT:-}" ]; then
    echo "FRANKENTERM_GUI, FRANKENTERM_CLI, and FRANKENTERM_CANDIDATE_ROOT must be explicit absolute candidate paths" >&2
    exit 2
fi
if [ -z "${FRANKENTERM_CANDIDATE_SHA:-}" ] || \
   [ "${#FRANKENTERM_CANDIDATE_SHA}" -ne 40 ] || \
   [[ "$FRANKENTERM_CANDIDATE_SHA" == *[!0-9a-f]* ]]; then
    echo "FRANKENTERM_CANDIDATE_SHA must be one lowercase full 40-hex source SHA" >&2
    exit 2
fi
if [ -z "${FRANKENTERM_BUILD_PROFILE:-}" ]; then
    echo "FRANKENTERM_BUILD_PROFILE must identify the candidate build profile" >&2
    exit 2
fi

FT_GUI="$FRANKENTERM_GUI"
FT_CLI="$FRANKENTERM_CLI"
CANDIDATE_ROOT="$FRANKENTERM_CANDIDATE_ROOT"
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

for candidate_path in "$FT_GUI" "$FT_CLI" "$CANDIDATE_ROOT"; do
    case "$candidate_path" in
        /*) ;;
        *) echo "candidate paths must be absolute: $candidate_path" >&2; exit 2 ;;
    esac
done
if [ ! -d "$CANDIDATE_ROOT" ]; then
    echo "candidate root is not a directory: $CANDIDATE_ROOT" >&2
    exit 2
fi
CANDIDATE_ROOT=$(cd "$CANDIDATE_ROOT" && pwd -P)
for candidate_binary in "$FT_GUI" "$FT_CLI"; do
    if [ ! -f "$candidate_binary" ] || [ ! -x "$candidate_binary" ] || [ -L "$candidate_binary" ]; then
        echo "candidate binary must be an executable non-symlink file: $candidate_binary" >&2
        exit 2
    fi
    candidate_directory=$(cd "$(dirname "$candidate_binary")" && pwd -P)
    case "$candidate_directory/" in
        "$CANDIDATE_ROOT"/*) ;;
        *) echo "candidate binary escapes candidate root: $candidate_binary" >&2; exit 2 ;;
    esac
done

hash_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        echo "no SHA-256 utility available" >&2
        return 1
    fi
}

GUI_SHA256=$(hash_file "$FT_GUI")
CLI_SHA256=$(hash_file "$FT_CLI")

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
    [ -n "${GUI_PID:-}" ] && stop_child "$GUI_PID" "GUI" || true
    [ -n "${WATCH_PID:-}" ] && stop_child "$WATCH_PID" "watch" || true
    echo "[cleanup] Logs in $LOG_DIR"
}

wait_child_bounded() {
    local pid="$1"
    local attempts=50
    while [ "$attempts" -gt 0 ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            return 0
        fi
        attempts=$((attempts - 1))
        sleep 0.1
    done
    return 1
}

stop_child() {
    local pid="$1"
    local label="$2"
    case "$pid" in
        ''|*[!0-9]*) echo "invalid harness-owned $label pid: $pid" >&2; return 1 ;;
    esac
    if ! kill -0 "$pid" 2>/dev/null; then
        wait "$pid" 2>/dev/null || true
        return 0
    fi
    kill -TERM "$pid" 2>/dev/null || true
    if wait_child_bounded "$pid"; then
        return 0
    fi
    echo "[cleanup] $label pid $pid missed TERM deadline; escalating exact child to KILL" >&2
    kill -KILL "$pid" 2>/dev/null || true
    wait_child_bounded "$pid"
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
echo "Candidate source SHA: $FRANKENTERM_CANDIDATE_SHA"
echo "Candidate profile: $FRANKENTERM_BUILD_PROFILE"
echo ""

{
    printf 'declared_source_sha=%s\n' "$FRANKENTERM_CANDIDATE_SHA"
    printf 'build_profile=%s\n' "$FRANKENTERM_BUILD_PROFILE"
    printf 'cli_path=%s\ncli_sha256=%s\n' "$FT_CLI" "$CLI_SHA256"
    printf 'gui_path=%s\ngui_sha256=%s\n' "$FT_GUI" "$GUI_SHA256"
} >"$LOG_DIR/artifact-manifest.txt"

# Step 1: Start ft watch in foreground mode with an isolated, explicitly
# enabled native-event configuration. The listener exclusively owns
# authenticated, identity-pinned stale-socket cleanup; this harness must never
# unlink a caller-selected path.
echo "[step 1] Starting ft watch..."
(
    cd "$WORKSPACE_DIR"
    exec env RUST_LOG=info,frankenterm_core::native_events=debug WEZTERM_FT_SOCKET="$SOCKET_PATH" \
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
if grep -q "Native event bridge: authenticated socket connected" "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
else
    check "GUI connected to native event socket" "fail"
fi

# Step 4: Check ft watch completed the bind. The earlier "Starting" marker is
# deliberately insufficient: it is emitted before bind and is also followed by
# the polling-only fallback when bind fails.
if grep -Fq "Native event listener bound" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
    check "ft watch bound explicitly enabled native listener" "pass"
else
    check "ft watch bound explicitly enabled native listener" "fail"
fi

# Step 4b: Prove the server, not just the GUI client, accepted the connection
# and decoded the GUI's protocol Hello frame.
if grep -Fq "native event connection accepted (cx path)" "$LOG_DIR/watch-stderr.log" 2>/dev/null && \
   grep -Fq "native event protocol hello received" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
    check "ft watch authenticated connection and decoded Hello" "pass"
else
    check "ft watch authenticated connection and decoded Hello" "fail"
fi

# Step 5: Stop the harness-owned GUI and verify ft watch stays alive.
echo "[step 5] Stopping harness-owned GUI, checking server-observed disconnect..."
if stop_child "$GUI_PID" "GUI"; then
    check "harness-owned GUI stopped within bounded cleanup" "pass"
else
    check "harness-owned GUI stopped within bounded cleanup" "fail"
fi
unset GUI_PID

disconnect_seen=false
for _ in $(seq 1 40); do
    if grep -Fq "native event connection closed (cx path)" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
        disconnect_seen=true
        break
    fi
    sleep 0.1
done
if [ "$disconnect_seen" = true ]; then
    check "ft watch observed GUI connection close" "pass"
else
    check "ft watch observed GUI connection close" "fail"
fi

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
