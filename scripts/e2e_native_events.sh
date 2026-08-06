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
#   - Its detached atomic manifest in FRANKENTERM_COMPONENT_MANIFEST
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
#   2 = launch refused by authorization or candidate-integrity preflight

if [[ ${BASH_SOURCE[0]} != "$0" ]]; then
    printf '%s\n' \
        'refusing to source e2e_native_events.sh; execute it in a dedicated process' >&2
    return 2
fi

set -euo pipefail
umask 077
SANITIZED_PATH=/usr/bin:/bin:/usr/sbin:/sbin
PATH=$SANITIZED_PATH
export PATH

if [ "${FRANKENTERM_ALLOW_GUI_E2E:-}" != "1" ]; then
    echo "refusing to launch FrankenTerm GUI without FRANKENTERM_ALLOW_GUI_E2E=1" >&2
    exit 2
fi
if [ "${FRANKENTERM_GUI_E2E_FOCUS_ACK:-}" != "I_ACCEPT_FOCUS_DISRUPTION" ]; then
    echo "refusing GUI launch without FRANKENTERM_GUI_E2E_FOCUS_ACK=I_ACCEPT_FOCUS_DISRUPTION" >&2
    exit 2
fi

if [ -z "${FRANKENTERM_GUI:-}" ] || [ -z "${FRANKENTERM_CLI:-}" ] || \
   [ -z "${FRANKENTERM_CANDIDATE_ROOT:-}" ] || \
   [ -z "${FRANKENTERM_COMPONENT_MANIFEST:-}" ]; then
    echo "FRANKENTERM_GUI, FRANKENTERM_CLI, FRANKENTERM_CANDIDATE_ROOT, and FRANKENTERM_COMPONENT_MANIFEST must be explicit absolute candidate paths" >&2
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
CANDIDATE_MANIFEST="$FRANKENTERM_COMPONENT_MANIFEST"
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
ATOMIC_MANIFEST_TOOL="$SCRIPT_DIR/atomic-component-manifest.sh"
LOG_DIR=$(mktemp -d /tmp/e2e-native-events.XXXXXX)
WORKSPACE_DIR="$LOG_DIR/workspace"
NATIVE_RUNTIME_DIR="$WORKSPACE_DIR/native-runtime"
SOCKET_PATH="$NATIVE_RUNTIME_DIR/events.sock"
CONFIG_PATH="$WORKSPACE_DIR/ft.toml"
HERMETIC_HOME="$WORKSPACE_DIR/home"
HERMETIC_XDG_CONFIG_HOME="$WORKSPACE_DIR/xdg-config"
HERMETIC_XDG_CACHE_HOME="$WORKSPACE_DIR/xdg-cache"
HERMETIC_XDG_DATA_HOME="$WORKSPACE_DIR/xdg-data"
HERMETIC_XDG_STATE_HOME="$WORKSPACE_DIR/xdg-state"
HERMETIC_XDG_RUNTIME_DIR="$WORKSPACE_DIR/xdg-runtime"
HERMETIC_TMPDIR="$WORKSPACE_DIR/tmp"
PRIVATE_MUX_DIR="$WORKSPACE_DIR/private-mux"
PRIVATE_MUX_SOCKET_PATH="$PRIVATE_MUX_DIR/not-created.sock"
PASS=0
FAIL=0
# Never inherit process authority from the invoking environment. Only the
# exact PIDs assigned from this harness's own background launches may reach
# the EXIT cleanup path.
GUI_PID=''
WATCH_PID=''

case "$(uname -s)" in
    Darwin|Linux|FreeBSD|DragonFly) ;;
    *)
        echo "native event E2E requires an authenticated Unix peer-credential target" >&2
        exit 1
        ;;
esac

for candidate_path in "$FT_GUI" "$FT_CLI" "$CANDIDATE_ROOT" "$CANDIDATE_MANIFEST"; do
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
FT_GUI=$(cd "$(dirname "$FT_GUI")" && pwd -P)/$(basename "$FT_GUI")
FT_CLI=$(cd "$(dirname "$FT_CLI")" && pwd -P)/$(basename "$FT_CLI")
if [ ! -f "$CANDIDATE_MANIFEST" ] || [ -L "$CANDIDATE_MANIFEST" ]; then
    echo "candidate component manifest must be a regular non-symlink file: $CANDIDATE_MANIFEST" >&2
    exit 2
fi
CANDIDATE_MANIFEST=$(cd "$(dirname "$CANDIDATE_MANIFEST")" && pwd -P)/$(basename "$CANDIDATE_MANIFEST")
case "$CANDIDATE_MANIFEST" in
    "$CANDIDATE_ROOT"|"$CANDIDATE_ROOT"/*)
        echo "candidate component manifest must be detached from the candidate root: $CANDIDATE_MANIFEST" >&2
        exit 2
        ;;
esac
if [ ! -f "$ATOMIC_MANIFEST_TOOL" ] || [ -L "$ATOMIC_MANIFEST_TOOL" ]; then
    echo "atomic component verifier must be a regular non-symlink file: $ATOMIC_MANIFEST_TOOL" >&2
    exit 2
fi

DECLARED_CANDIDATE_ROOT="$CANDIDATE_ROOT"
DECLARED_FT_GUI="$FT_GUI"
DECLARED_FT_CLI="$FT_CLI"
FT_GUI_RELATIVE=${FT_GUI#"$CANDIDATE_ROOT"/}
FT_CLI_RELATIVE=${FT_CLI#"$CANDIDATE_ROOT"/}

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

verify_atomic_candidate_root() {
    env -i \
        "PATH=$SANITIZED_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        bash "$ATOMIC_MANIFEST_TOOL" verify \
        --root "$CANDIDATE_ROOT" \
        --manifest "$CANDIDATE_MANIFEST"
}

verify_candidate_manifest_contract() {
    env -i \
        "PATH=$SANITIZED_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        python3 - \
        "$CANDIDATE_MANIFEST" \
        "$FRANKENTERM_CANDIDATE_SHA" \
        "$FRANKENTERM_BUILD_PROFILE" \
        "$CANDIDATE_ROOT" \
        "$FT_CLI" \
        "$FT_GUI" \
        "$CLI_SHA256" \
        "$GUI_SHA256" <<'PY'
import json
import sys
from pathlib import Path


def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key!r}")
        result[key] = value
    return result


def fail(message):
    raise SystemExit(f"candidate manifest contract mismatch: {message}")


manifest_path = Path(sys.argv[1])
expected_source_revision = sys.argv[2]
expected_profile = sys.argv[3]
package_root = Path(sys.argv[4]).resolve(strict=True)
expected_components = {
    "ft": (Path(sys.argv[5]).resolve(strict=True), sys.argv[7]),
    "frankenterm-gui": (Path(sys.argv[6]).resolve(strict=True), sys.argv[8]),
}

try:
    manifest = json.loads(
        manifest_path.read_bytes().decode("utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )
except (OSError, UnicodeError, ValueError, json.JSONDecodeError) as error:
    fail(f"cannot read strict JSON from {manifest_path}: {error}")

if not isinstance(manifest, dict):
    fail("top-level value is not an object")
identity = manifest.get("identity")
if not isinstance(identity, dict):
    fail("identity is not an object")
if identity.get("source_revision") != expected_source_revision:
    fail(
        "source revision differs "
        f"(expected {expected_source_revision!r}, observed {identity.get('source_revision')!r})"
    )
if identity.get("profile") != expected_profile:
    fail(
        "build profile differs "
        f"(expected {expected_profile!r}, observed {identity.get('profile')!r})"
    )

files = manifest.get("files")
if not isinstance(files, list):
    fail("files is not an array")

verified_paths = {}
for component, (binary_path, binary_sha256) in expected_components.items():
    try:
        expected_relative = binary_path.relative_to(package_root).as_posix()
    except ValueError:
        fail(f"{component} path escapes the package root: {binary_path}")
    matches = [
        record
        for record in files
        if isinstance(record, dict) and record.get("component") == component
    ]
    if len(matches) != 1:
        fail(f"component {component!r} has {len(matches)} manifest records, expected exactly one")
    record = matches[0]
    if record.get("path") != expected_relative:
        fail(
            f"component {component!r} path differs "
            f"(expected {expected_relative!r}, observed {record.get('path')!r})"
        )
    if record.get("kind") != "executable" or record.get("executable") is not True:
        fail(f"component {component!r} is not catalogued as an executable file")
    if record.get("sha256") != binary_sha256:
        fail(
            f"component {component!r} digest differs "
            f"(expected {binary_sha256!r}, observed {record.get('sha256')!r})"
        )
    verified_paths[component] = expected_relative

print(
    json.dumps(
        {
            "ok": True,
            "profile": expected_profile,
            "source_revision": expected_source_revision,
            "verified_component_paths": verified_paths,
        },
        sort_keys=True,
    )
)
PY
}

GUI_SHA256=$(hash_file "$FT_GUI")
CLI_SHA256=$(hash_file "$FT_CLI")
MANIFEST_SHA256_BEFORE=$(hash_file "$CANDIDATE_MANIFEST")

# Fail closed before either candidate binary is started. The offline verifier
# binds the exact package inventory, executable bytes, and embedded component
# identities to this detached manifest. The additional contract check binds
# the operator-supplied source SHA/profile and exact ft/GUI paths to it.
if ! verify_atomic_candidate_root \
    >"$LOG_DIR/atomic-manifest-verify.json" \
    2>"$LOG_DIR/atomic-manifest-verify.stderr"; then
    echo "candidate atomic component manifest verification failed; refusing process launch" >&2
    sed -n '1,120p' "$LOG_DIR/atomic-manifest-verify.stderr" >&2
    exit 2
fi
if ! verify_candidate_manifest_contract \
    >"$LOG_DIR/candidate-manifest-contract.json" \
    2>"$LOG_DIR/candidate-manifest-contract.stderr"; then
    echo "candidate manifest identity/path contract failed; refusing process launch" >&2
    sed -n '1,120p' "$LOG_DIR/candidate-manifest-contract.stderr" >&2
    exit 2
fi

MANIFEST_SHA256_AFTER=$(hash_file "$CANDIDATE_MANIFEST")
GUI_SHA256_AFTER=$(hash_file "$FT_GUI")
CLI_SHA256_AFTER=$(hash_file "$FT_CLI")
if [ "$MANIFEST_SHA256_AFTER" != "$MANIFEST_SHA256_BEFORE" ] || \
   [ "$GUI_SHA256_AFTER" != "$GUI_SHA256" ] || \
   [ "$CLI_SHA256_AFTER" != "$CLI_SHA256" ]; then
    echo "candidate manifest or executable changed during verification; refusing process launch" >&2
    exit 2
fi


# Execute only a harness-owned snapshot, never paths in the caller-owned
# candidate tree. The copied tree is verified again as a complete package
# before either process starts, closing the verify-then-exec race against the
# original staging directory and keeping all runtime package access inside the
# private 0700 harness directory.
EXECUTION_CANDIDATE_ROOT="$LOG_DIR/candidate-snapshot"
mkdir -m 700 "$EXECUTION_CANDIDATE_ROOT"
if ! cp -Rp "$DECLARED_CANDIDATE_ROOT/." "$EXECUTION_CANDIDATE_ROOT/"; then
    echo "failed to create private candidate snapshot; refusing process launch" >&2
    exit 2
fi
CANDIDATE_ROOT=$(cd "$EXECUTION_CANDIDATE_ROOT" && pwd -P)
FT_GUI="$CANDIDATE_ROOT/$FT_GUI_RELATIVE"
FT_CLI="$CANDIDATE_ROOT/$FT_CLI_RELATIVE"
for snapshot_binary in "$FT_GUI" "$FT_CLI"; do
    if [ ! -f "$snapshot_binary" ] || [ ! -x "$snapshot_binary" ] || [ -L "$snapshot_binary" ]; then
        echo "private candidate snapshot lost executable identity: $snapshot_binary" >&2
        exit 2
    fi
done
if ! verify_atomic_candidate_root \
    >"$LOG_DIR/snapshot-atomic-manifest-verify.json" \
    2>"$LOG_DIR/snapshot-atomic-manifest-verify.stderr"; then
    echo "private candidate snapshot failed atomic manifest verification; refusing process launch" >&2
    sed -n '1,120p' "$LOG_DIR/snapshot-atomic-manifest-verify.stderr" >&2
    exit 2
fi
if ! verify_candidate_manifest_contract \
    >"$LOG_DIR/snapshot-manifest-contract.json" \
    2>"$LOG_DIR/snapshot-manifest-contract.stderr"; then
    echo "private candidate snapshot failed identity/path verification; refusing process launch" >&2
    sed -n '1,120p' "$LOG_DIR/snapshot-manifest-contract.stderr" >&2
    exit 2
fi
if [ "$(hash_file "$CANDIDATE_MANIFEST")" != "$MANIFEST_SHA256_AFTER" ] || \
   [ "$(hash_file "$FT_GUI")" != "$GUI_SHA256" ] || \
   [ "$(hash_file "$FT_CLI")" != "$CLI_SHA256" ]; then
    echo "private candidate snapshot bytes differ from the verified candidate; refusing process launch" >&2
    exit 2
fi

verify_execution_snapshot_integrity() {
    local phase="$1"
    if [ "$(hash_file "$CANDIDATE_MANIFEST")" != "$MANIFEST_SHA256_AFTER" ] || \
       [ "$(hash_file "$FT_GUI")" != "$GUI_SHA256" ] || \
       [ "$(hash_file "$FT_CLI")" != "$CLI_SHA256" ]; then
        echo "candidate snapshot identity changed during $phase" >&2
        return 1
    fi
    if ! verify_atomic_candidate_root \
        >"$LOG_DIR/snapshot-atomic-$phase.json" \
        2>"$LOG_DIR/snapshot-atomic-$phase.stderr"; then
        echo "candidate snapshot package verification failed during $phase" >&2
        return 1
    fi
    if ! verify_candidate_manifest_contract \
        >"$LOG_DIR/snapshot-contract-$phase.json" \
        2>"$LOG_DIR/snapshot-contract-$phase.stderr"; then
        echo "candidate snapshot component contract failed during $phase" >&2
        return 1
    fi
}

mkdir -m 700 \
    "$WORKSPACE_DIR" \
    "$NATIVE_RUNTIME_DIR" \
    "$HERMETIC_HOME" \
    "$HERMETIC_XDG_CONFIG_HOME" \
    "$HERMETIC_XDG_CACHE_HOME" \
    "$HERMETIC_XDG_DATA_HOME" \
    "$HERMETIC_XDG_STATE_HOME" \
    "$HERMETIC_XDG_RUNTIME_DIR" \
    "$HERMETIC_TMPDIR" \
    "$PRIVATE_MUX_DIR"
{
    echo '[native]'
    echo 'enabled = true'
    printf 'socket_path = "%s"\n' "$SOCKET_PATH"
    echo ''
    echo '[vendored]'
    printf 'mux_socket_path = "%s"\n' "$PRIVATE_MUX_SOCKET_PATH"
} >"$CONFIG_PATH"

BASE_HERMETIC_ENV=(
    "PATH=$SANITIZED_PATH"
    "LANG=${LANG:-C}"
    "HOME=$HERMETIC_HOME"
    "XDG_CONFIG_HOME=$HERMETIC_XDG_CONFIG_HOME"
    "XDG_CACHE_HOME=$HERMETIC_XDG_CACHE_HOME"
    "XDG_DATA_HOME=$HERMETIC_XDG_DATA_HOME"
    "XDG_STATE_HOME=$HERMETIC_XDG_STATE_HOME"
    "XDG_RUNTIME_DIR=$HERMETIC_XDG_RUNTIME_DIR"
    "TMPDIR=$HERMETIC_TMPDIR"
    "FRANKENTERM_CONFIG_FILE=$CONFIG_PATH"
    "WEZTERM_CONFIG_FILE=$CONFIG_PATH"
    "FRANKENTERM_CONFIG_DIR=$WORKSPACE_DIR"
    "WEZTERM_CONFIG_DIR=$WORKSPACE_DIR"
    "FRANKENTERM_UNIX_SOCKET=$PRIVATE_MUX_SOCKET_PATH"
    "WEZTERM_UNIX_SOCKET=$PRIVATE_MUX_SOCKET_PATH"
    "FT_WEZTERM_CLI=$PRIVATE_MUX_DIR/not-created-cli"
    "FT_WORKSPACE=$WORKSPACE_DIR"
    "FT_STORAGE_DB_PATH=$WORKSPACE_DIR/frankenterm.db"
    "FT_METRICS_ENABLED=false"
)

# The watcher receives only BASE_HERMETIC_ENV and therefore has no display,
# compositor, X11 authorization, Cocoa-preference, or desktop-session
# authority. Retain graphical coordinates solely for the explicitly authorized
# GUI child. All unrelated caller state is excluded by `env -i`.
GUI_HERMETIC_ENV=(
    "${BASE_HERMETIC_ENV[@]}"
    "CFFIXED_USER_HOME=$HERMETIC_HOME"
)
if [ -n "${DISPLAY:-}" ]; then
    GUI_HERMETIC_ENV+=("DISPLAY=$DISPLAY")
fi
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
    case "$WAYLAND_DISPLAY" in
        /*)
            GUI_HERMETIC_ENV+=("WAYLAND_DISPLAY=$WAYLAND_DISPLAY")
            ;;
        *)
            case "${XDG_RUNTIME_DIR:-}" in
                /*)
                    # libwayland accepts an absolute display socket. Preserve
                    # that one intentional graphical-session connection while
                    # keeping the child's own XDG_RUNTIME_DIR private.
                    GUI_HERMETIC_ENV+=("WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY")
                    ;;
                *)
                    echo "relative WAYLAND_DISPLAY cannot be resolved without an absolute caller XDG_RUNTIME_DIR; refusing process launch" >&2
                    exit 2
                    ;;
            esac
            ;;
    esac
fi
if [ -n "${XAUTHORITY:-}" ]; then
    GUI_HERMETIC_ENV+=("XAUTHORITY=$XAUTHORITY")
fi
if [ -n "${XDG_SESSION_TYPE:-}" ]; then
    GUI_HERMETIC_ENV+=("XDG_SESSION_TYPE=$XDG_SESSION_TYPE")
fi
if [ -n "${XDG_CURRENT_DESKTOP:-}" ]; then
    GUI_HERMETIC_ENV+=("XDG_CURRENT_DESKTOP=$XDG_CURRENT_DESKTOP")
fi

assert_private_mux_socket_absent() {
    if [ -e "$PRIVATE_MUX_SOCKET_PATH" ] || [ -L "$PRIVATE_MUX_SOCKET_PATH" ]; then
        echo "private watcher mux socket unexpectedly exists; refusing process launch: $PRIVATE_MUX_SOCKET_PATH" >&2
        return 1
    fi
}

# Invoked through the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
    local original_status=$?
    local cleanup_failed=0
    # The cleanup path must run to completion even when stdout is closed or a
    # child signal/wait fails. Disable recursive EXIT trapping, settle the
    # harness-owned children first, then verify the execution snapshot and
    # emit best-effort diagnostics. A successful test must never conceal a
    # child that escaped settlement or candidate bytes that changed in flight.
    trap - EXIT
    set +e
    if [ -n "${GUI_PID:-}" ] && ! stop_child "$GUI_PID" "GUI"; then
        printf '%s\n' "[cleanup] GUI child settlement failed for pid $GUI_PID" >&2
        cleanup_failed=1
    fi
    if [ -n "${WATCH_PID:-}" ] && ! stop_child "$WATCH_PID" "watch"; then
        printf '%s\n' "[cleanup] watch child settlement failed for pid $WATCH_PID" >&2
        cleanup_failed=1
    fi
    if ! verify_execution_snapshot_integrity after-child-settlement; then
        printf '%s\n' '[cleanup] candidate snapshot post-execution verification failed' >&2
        cleanup_failed=1
    fi
    printf '%s\n' "[cleanup] Harness-owned child settlement completed" >&2
    printf '%s\n' "[cleanup] Logs in $LOG_DIR" >&2
    if [ "$original_status" -eq 0 ] && [ "$cleanup_failed" -ne 0 ]; then
        original_status=1
    fi
    exit "$original_status"
}

wait_child_bounded() {
    local pid="$1"
    local attempts=50
    while [ "$attempts" -gt 0 ]; do
        if ! builtin kill -0 "$pid" 2>/dev/null; then
            builtin wait "$pid" 2>/dev/null || true
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
    if [ "$pid" -le 1 ]; then
        echo "refusing unsafe harness-owned $label pid: $pid" >&2
        return 1
    fi
    if ! builtin kill -0 "$pid" 2>/dev/null; then
        builtin wait "$pid" 2>/dev/null || true
        return 0
    fi
    builtin kill -TERM "$pid" 2>/dev/null || true
    if wait_child_bounded "$pid"; then
        return 0
    fi
    echo "[cleanup] $label pid $pid missed TERM deadline; escalating exact child to KILL" >&2
    builtin kill -KILL "$pid" 2>/dev/null || true
    wait_child_bounded "$pid"
}

wait_for_watch_bind() {
    local attempts=100
    while [ "$attempts" -gt 0 ]; do
        if grep -Fq \
            "Failed to bind native event socket, falling back to polling only" \
            "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
            return 2
        fi
        if grep -Fq \
            "Native event listener bound — waiting for GUI connections" \
            "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
            builtin kill -0 "$WATCH_PID" 2>/dev/null
            return
        fi
        if ! builtin kill -0 "$WATCH_PID" 2>/dev/null; then
            builtin wait "$WATCH_PID" 2>/dev/null || true
            return 1
        fi
        attempts=$((attempts - 1))
        sleep 0.1
    done
    return 3
}

wait_for_bridge_handshake() {
    local attempts=150
    local gui_connected_marker="Native event bridge: authenticated socket connected at $SOCKET_PATH"
    while [ "$attempts" -gt 0 ]; do
        if ! builtin kill -0 "$WATCH_PID" 2>/dev/null; then
            builtin wait "$WATCH_PID" 2>/dev/null || true
            return 1
        fi
        if ! builtin kill -0 "$GUI_PID" 2>/dev/null; then
            builtin wait "$GUI_PID" 2>/dev/null || true
            return 2
        fi
        if grep -Fq "$gui_connected_marker" "$LOG_DIR/gui-stderr.log" 2>/dev/null && \
           grep -Fq "native event connection accepted (cx path)" "$LOG_DIR/watch-stderr.log" 2>/dev/null && \
           grep -Fq "native event protocol hello received" "$LOG_DIR/watch-stderr.log" 2>/dev/null; then
            return 0
        fi
        attempts=$((attempts - 1))
        sleep 0.1
    done
    return 3
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
echo "Atomic manifest: $CANDIDATE_MANIFEST"
echo "Candidate source SHA: $FRANKENTERM_CANDIDATE_SHA"
echo "Candidate profile: $FRANKENTERM_BUILD_PROFILE"
echo ""

{
    printf 'declared_source_sha=%s\n' "$FRANKENTERM_CANDIDATE_SHA"
    printf 'build_profile=%s\n' "$FRANKENTERM_BUILD_PROFILE"
    printf 'component_manifest_path=%s\ncomponent_manifest_sha256=%s\n' \
        "$CANDIDATE_MANIFEST" "$MANIFEST_SHA256_AFTER"
    printf 'declared_candidate_root=%s\nexecution_candidate_root=%s\n' \
        "$DECLARED_CANDIDATE_ROOT" "$CANDIDATE_ROOT"
    printf 'declared_cli_path=%s\nexecution_cli_path=%s\ncli_sha256=%s\n' \
        "$DECLARED_FT_CLI" "$FT_CLI" "$CLI_SHA256"
    printf 'declared_gui_path=%s\nexecution_gui_path=%s\ngui_sha256=%s\n' \
        "$DECLARED_FT_GUI" "$FT_GUI" "$GUI_SHA256"
} >"$LOG_DIR/artifact-manifest.txt"

# Step 1: Start ft watch in foreground mode with an isolated, explicitly
# enabled native-event configuration. The listener exclusively owns
# authenticated, identity-pinned stale-socket cleanup; this harness must never
# unlink a caller-selected path.
echo "[step 1] Starting ft watch..."
assert_private_mux_socket_absent
if ! verify_execution_snapshot_integrity before-watch; then
    echo "candidate snapshot changed before watch launch; refusing process launch" >&2
    exit 2
fi
(
    cd "$WORKSPACE_DIR"
    exec env -i "${BASE_HERMETIC_ENV[@]}" \
        RUST_LOG=info,frankenterm_core::native_events=debug \
        WEZTERM_FT_SOCKET="$SOCKET_PATH" \
        "$FT_CLI" --config "$CONFIG_PATH" --workspace "$WORKSPACE_DIR" watch --foreground
) >"$LOG_DIR/watch-stdout.log" 2>"$LOG_DIR/watch-stderr.log" &
WATCH_PID=$!
if wait_for_watch_bind; then
    check "ft watch started" "pass"
    check "ft watch bound explicitly enabled native listener" "pass"
else
    watch_bind_status=$?
    check "ft watch started" "fail"
    case "$watch_bind_status" in
        2) echo "ft watch reported native-listener bind failure before GUI launch" >&2 ;;
        3) echo "ft watch did not prove native-listener readiness before the bounded deadline" >&2 ;;
        *) echo "ft watch exited before proving native-listener readiness" >&2 ;;
    esac
    echo "Check $LOG_DIR/watch-stderr.log" >&2
    exit 1
fi

# Step 2: Start frankenterm-gui from the same isolated directory. Its ft-core
# configuration loader reads ./ft.toml, while both supported config-file
# environment names point to that same private file.
echo "[step 2] Starting frankenterm-gui..."
assert_private_mux_socket_absent
if ! verify_execution_snapshot_integrity before-gui; then
    echo "candidate snapshot changed before GUI launch; refusing process launch" >&2
    exit 2
fi
(
    cd "$WORKSPACE_DIR"
    exec env -i "${GUI_HERMETIC_ENV[@]}" \
        RUST_LOG=info \
        WEZTERM_FT_SOCKET="$SOCKET_PATH" \
        "$FT_GUI" --skip-config --config check_for_updates=false \
        start --always-new-process --no-auto-connect -- /bin/cat
) >"$LOG_DIR/gui-stdout.log" 2>"$LOG_DIR/gui-stderr.log" &
GUI_PID=$!
if wait_for_bridge_handshake; then
    check "frankenterm-gui started" "pass"
else
    bridge_status=$?
    check "frankenterm-gui started" "fail"
    case "$bridge_status" in
        1) echo "ft watch exited while waiting for the authenticated GUI handshake" >&2 ;;
        2) echo "GUI exited before completing the authenticated native-event handshake" >&2 ;;
        3) echo "GUI/server handshake did not complete before the bounded deadline" >&2 ;;
        *) echo "unexpected bridge readiness failure: $bridge_status" >&2 ;;
    esac
    echo "Check $LOG_DIR/gui-stderr.log and $LOG_DIR/watch-stderr.log" >&2
    exit 1
fi

# Step 3: Check that the authenticated native event bridge connected.
if grep -Fq \
    "Native event bridge: authenticated socket connected at $SOCKET_PATH" \
    "$LOG_DIR/gui-stderr.log" 2>/dev/null; then
    check "GUI connected to native event socket" "pass"
else
    check "GUI connected to native event socket" "fail"
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
    unset GUI_PID
else
    check "harness-owned GUI stopped within bounded cleanup" "fail"
    echo "GUI did not stop cleanly; retaining GUI_PID for EXIT cleanup" >&2
    exit 1
fi

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

if builtin kill -0 "$WATCH_PID" 2>/dev/null; then
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
