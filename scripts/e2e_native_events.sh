#!/usr/bin/env bash
# e2e_native_events.sh — End-to-end validation of the native event bridge.
#
# Tests authenticated native-bridge connection and disconnect lifecycle.
# Raw pane-output bytes are not emitted by the current GUI bridge; polling
# remains the authoritative text-capture path.
#
# Prerequisites:
#   - Explicit absolute candidate paths for all four shipped process roles in
#     FRANKENTERM_CLI, FRANKENTERM_GUI, FRANKENTERM_MUX_SERVER, and
#     FRANKENTERM_PTY_GUARDIAN
#   - Their common candidate root in FRANKENTERM_CANDIDATE_ROOT
#   - Its detached atomic manifest in FRANKENTERM_COMPONENT_MANIFEST
#   - Full source SHA/profile metadata for the retained artifact manifest
#   - A graphical session in which launching a non-activating GUI is acceptable
#
# By default this test opens a real AppKit window behind existing windows and
# verifies that the frontmost application did not change. The explicit focus
# acknowledgement selects the separate focus-disrupting lane; automation must
# never manufacture that acknowledgement.
#
# Exit codes:
#   0 = all checks passed
#   1 = one or more checks failed
#   2 = launch refused by authorization or candidate-integrity preflight

if [[ ${BASH_SOURCE[0]} != "$0" ]]; then
    builtin printf '%s\n' \
        'refusing to source e2e_native_events.sh; execute it in a dedicated process' >&2
    return 2
fi
if [[ ${FRANKENTERM_ALLOW_GUI_E2E:-} != "1" ]]; then
    builtin printf '%s\n' \
        'refusing to launch FrankenTerm GUI without FRANKENTERM_ALLOW_GUI_E2E=1' >&2
    builtin exit 2
fi
GUI_E2E_MODE=focus-disrupting
if [[ $(uname -s) == Darwin && \
      ${FRANKENTERM_GUI_E2E_FOCUS_ACK:-} != "I_ACCEPT_FOCUS_DISRUPTION" ]]; then
    GUI_E2E_MODE=nonactivating
elif [[ ${FRANKENTERM_GUI_E2E_FOCUS_ACK:-} != "I_ACCEPT_FOCUS_DISRUPTION" ]]; then
    builtin printf '%s\n' \
        'non-macOS GUI launch still requires FRANKENTERM_GUI_E2E_FOCUS_ACK=I_ACCEPT_FOCUS_DISRUPTION' >&2
    builtin exit 2
fi
if [[ ${FRANKENTERM_GUI_E2E_FOCUS_ACK:-} == "I_ACCEPT_FOCUS_DISRUPTION" ]]; then
    GUI_E2E_MODE=focus-disrupting
fi

# Exported Bash functions must not be able to shadow `env`, `grep`, `cp`, or
# other commands before the scrubbed child environments are established.
while IFS= builtin read -r inherited_function; do
    builtin unset -f "$inherited_function"
done < <(builtin compgen -A function)

set -euo pipefail
umask 077
CDPATH=''
SANITIZED_PATH=/usr/bin:/bin:/usr/sbin:/sbin
ENV_BIN=/usr/bin/env
SLEEP_BIN=/bin/sleep
BASH_BIN=${BASH:-}
for required_runtime in "$ENV_BIN" "$SLEEP_BIN" "$BASH_BIN"; do
    case "$required_runtime" in
        /*) ;;
        *) printf 'invalid absolute harness runtime path: %s\n' "$required_runtime" >&2; exit 2 ;;
    esac
    if [ ! -f "$required_runtime" ] || [ ! -x "$required_runtime" ]; then
        printf 'missing harness runtime executable: %s\n' "$required_runtime" >&2
        exit 2
    fi
done
PYTHON3_BIN=''
PYTHON3_VERSION=''
for python_candidate in \
    /usr/bin/python3 \
    /usr/local/bin/python3 \
    /opt/homebrew/bin/python3; do
    if [ ! -f "$python_candidate" ] || [ ! -x "$python_candidate" ]; then
        continue
    fi
    if python_version=$("$ENV_BIN" -i "$python_candidate" -c \
        'import sys; assert sys.version_info >= (3, 10); print(".".join(map(str, sys.version_info[:3])))' \
        2>/dev/null); then
        PYTHON3_BIN=$python_candidate
        PYTHON3_VERSION=$python_version
        break
    fi
done
if [ -z "$PYTHON3_BIN" ]; then
    printf '%s\n' \
        'no Python >=3.10 interpreter found at an approved absolute system path' >&2
    exit 2
fi
PYTHON3_DIR=${PYTHON3_BIN%/*}
case "$PYTHON3_DIR" in
    /usr/bin|/bin|/usr/sbin|/sbin) PREFLIGHT_PATH=$SANITIZED_PATH ;;
    *) PREFLIGHT_PATH="$PYTHON3_DIR:$SANITIZED_PATH" ;;
esac
PATH=$SANITIZED_PATH
export PATH

if [ -z "${FRANKENTERM_GUI:-}" ] || [ -z "${FRANKENTERM_CLI:-}" ] || \
   [ -z "${FRANKENTERM_MUX_SERVER:-}" ] || \
   [ -z "${FRANKENTERM_PTY_GUARDIAN:-}" ] || \
   [ -z "${FRANKENTERM_CANDIDATE_ROOT:-}" ] || \
   [ -z "${FRANKENTERM_COMPONENT_MANIFEST:-}" ]; then
    echo "FRANKENTERM_GUI, FRANKENTERM_CLI, FRANKENTERM_MUX_SERVER, FRANKENTERM_PTY_GUARDIAN, FRANKENTERM_CANDIDATE_ROOT, and FRANKENTERM_COMPONENT_MANIFEST must be explicit absolute candidate paths" >&2
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
FT_MUX_SERVER="$FRANKENTERM_MUX_SERVER"
FT_PTY_GUARDIAN="$FRANKENTERM_PTY_GUARDIAN"
CANDIDATE_ROOT="$FRANKENTERM_CANDIDATE_ROOT"
CANDIDATE_MANIFEST="$FRANKENTERM_COMPONENT_MANIFEST"
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
ATOMIC_MANIFEST_TOOL="${FRANKENTERM_ATOMIC_MANIFEST_TOOL:-$SCRIPT_DIR/atomic-component-manifest.sh}"
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
GUI_LAUNCHED=0
WATCH_LAUNCHED=0
CANDIDATE_IDENTITY_SHA256=''
FRONTMOST_PID_BEFORE=''
MUX_VERSION_PROBED=0
GUARDIAN_VERSION_PROBED=0
PRIVATE_MUX_SOCKET_VIOLATION=0

case "$(uname -s)" in
    Darwin|Linux|FreeBSD|DragonFly) ;;
    *)
        echo "native event E2E requires an authenticated Unix peer-credential target" >&2
        exit 1
        ;;
esac

frontmost_application_pid() {
    local app_identity app_info pid
    app_identity=$(/usr/bin/lsappinfo front 2>/dev/null) || return 1
    [ -n "$app_identity" ] || return 1
    app_info=$(/usr/bin/lsappinfo info -only pid "$app_identity" 2>/dev/null) || return 1
    pid=${app_info#*=}
    case "$pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    printf '%s\n' "$pid"
}

if [ "$GUI_E2E_MODE" = nonactivating ]; then
    if [ ! -x /usr/bin/lsappinfo ]; then
        echo "non-activating GUI proof requires /usr/bin/lsappinfo" >&2
        exit 2
    fi
fi

for candidate_path in \
    "$FT_GUI" "$FT_CLI" "$FT_MUX_SERVER" "$FT_PTY_GUARDIAN" \
    "$CANDIDATE_ROOT" "$CANDIDATE_MANIFEST"; do
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
if [ "$CANDIDATE_ROOT" = "/" ]; then
    echo "candidate root may not be the filesystem root" >&2
    exit 2
fi
for candidate_binary in "$FT_GUI" "$FT_CLI" "$FT_MUX_SERVER" "$FT_PTY_GUARDIAN"; do
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
FT_MUX_SERVER=$(cd "$(dirname "$FT_MUX_SERVER")" && pwd -P)/$(basename "$FT_MUX_SERVER")
FT_PTY_GUARDIAN=$(cd "$(dirname "$FT_PTY_GUARDIAN")" && pwd -P)/$(basename "$FT_PTY_GUARDIAN")
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
DECLARED_FT_MUX_SERVER="$FT_MUX_SERVER"
DECLARED_FT_PTY_GUARDIAN="$FT_PTY_GUARDIAN"
DECLARED_CANDIDATE_MANIFEST="$CANDIDATE_MANIFEST"
DECLARED_ATOMIC_MANIFEST_TOOL="$ATOMIC_MANIFEST_TOOL"
FT_GUI_RELATIVE=${FT_GUI#"$CANDIDATE_ROOT"/}
FT_CLI_RELATIVE=${FT_CLI#"$CANDIDATE_ROOT"/}
FT_MUX_SERVER_RELATIVE=${FT_MUX_SERVER#"$CANDIDATE_ROOT"/}
FT_PTY_GUARDIAN_RELATIVE=${FT_PTY_GUARDIAN#"$CANDIDATE_ROOT"/}

hash_file() {
    "$ENV_BIN" -i \
        "PATH=$PREFLIGHT_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        "$PYTHON3_BIN" - "$1" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as source:
    while True:
        chunk = source.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
print(digest.hexdigest())
PY
}

verify_atomic_candidate_root() {
    "$ENV_BIN" -i \
        "PATH=$PREFLIGHT_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        "$BASH_BIN" "$ATOMIC_MANIFEST_TOOL" verify \
        --root "$CANDIDATE_ROOT" \
        --manifest "$CANDIDATE_MANIFEST"
}

verify_candidate_manifest_contract() {
    "$ENV_BIN" -i \
        "PATH=$PREFLIGHT_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        "$PYTHON3_BIN" - \
        "$CANDIDATE_MANIFEST" \
        "$FRANKENTERM_CANDIDATE_SHA" \
        "$FRANKENTERM_BUILD_PROFILE" \
        "$CANDIDATE_ROOT" \
        "$FT_CLI" \
        "$FT_GUI" \
        "$FT_MUX_SERVER" \
        "$FT_PTY_GUARDIAN" \
        "$CLI_SHA256" \
        "$GUI_SHA256" \
        "$MUX_SERVER_SHA256" \
        "$PTY_GUARDIAN_SHA256" <<'PY'
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
    "ft": (Path(sys.argv[5]).resolve(strict=True), sys.argv[9]),
    "frankenterm-gui": (Path(sys.argv[6]).resolve(strict=True), sys.argv[10]),
    "frankenterm-mux-server": (Path(sys.argv[7]).resolve(strict=True), sys.argv[11]),
    "frankenterm-pty-guardian": (Path(sys.argv[8]).resolve(strict=True), sys.argv[12]),
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
MUX_SERVER_SHA256=$(hash_file "$FT_MUX_SERVER")
PTY_GUARDIAN_SHA256=$(hash_file "$FT_PTY_GUARDIAN")
MANIFEST_SHA256_BEFORE=$(hash_file "$CANDIDATE_MANIFEST")
ATOMIC_MANIFEST_TOOL_SHA256=$(hash_file "$ATOMIC_MANIFEST_TOOL")

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
MUX_SERVER_SHA256_AFTER=$(hash_file "$FT_MUX_SERVER")
PTY_GUARDIAN_SHA256_AFTER=$(hash_file "$FT_PTY_GUARDIAN")
ATOMIC_MANIFEST_TOOL_SHA256_AFTER=$(hash_file "$ATOMIC_MANIFEST_TOOL")
if [ "$MANIFEST_SHA256_AFTER" != "$MANIFEST_SHA256_BEFORE" ] || \
   [ "$GUI_SHA256_AFTER" != "$GUI_SHA256" ] || \
   [ "$CLI_SHA256_AFTER" != "$CLI_SHA256" ] || \
   [ "$MUX_SERVER_SHA256_AFTER" != "$MUX_SERVER_SHA256" ] || \
   [ "$PTY_GUARDIAN_SHA256_AFTER" != "$PTY_GUARDIAN_SHA256" ] || \
   [ "$ATOMIC_MANIFEST_TOOL_SHA256_AFTER" != "$ATOMIC_MANIFEST_TOOL_SHA256" ]; then
    echo "candidate manifest, verifier, or executable changed during verification; refusing process launch" >&2
    exit 2
fi

# Execute only a harness-owned snapshot, never paths in the caller-owned
# candidate tree. The copied tree is verified again as a complete package
# before either process starts, closing the verify-then-exec race against the
# original staging directory and keeping all runtime package access inside the
# private 0700 harness directory.
EXECUTION_MANIFEST="$LOG_DIR/execution-component-manifest.json"
EXECUTION_ATOMIC_MANIFEST_TOOL="$LOG_DIR/execution-atomic-component-manifest.sh"
if ! cp -p "$DECLARED_CANDIDATE_MANIFEST" "$EXECUTION_MANIFEST" || \
   ! cp -p "$DECLARED_ATOMIC_MANIFEST_TOOL" "$EXECUTION_ATOMIC_MANIFEST_TOOL"; then
    echo "failed to snapshot detached manifest and verifier; refusing process launch" >&2
    exit 2
fi
if [ -L "$EXECUTION_MANIFEST" ] || [ -L "$EXECUTION_ATOMIC_MANIFEST_TOOL" ] || \
   [ "$(hash_file "$EXECUTION_MANIFEST")" != "$MANIFEST_SHA256_AFTER" ] || \
   [ "$(hash_file "$EXECUTION_ATOMIC_MANIFEST_TOOL")" != "$ATOMIC_MANIFEST_TOOL_SHA256" ]; then
    echo "private manifest or verifier snapshot differs from verified bytes; refusing process launch" >&2
    exit 2
fi
CANDIDATE_MANIFEST="$EXECUTION_MANIFEST"
ATOMIC_MANIFEST_TOOL="$EXECUTION_ATOMIC_MANIFEST_TOOL"

EXECUTION_CANDIDATE_PARENT="$LOG_DIR/candidate-snapshot"
CANDIDATE_ROOT_BASENAME=${DECLARED_CANDIDATE_ROOT##*/}
mkdir -m 700 "$EXECUTION_CANDIDATE_PARENT"
if ! cp -Rp "$DECLARED_CANDIDATE_ROOT" "$EXECUTION_CANDIDATE_PARENT/"; then
    echo "failed to create private candidate snapshot; refusing process launch" >&2
    exit 2
fi
CANDIDATE_ROOT=$(cd "$EXECUTION_CANDIDATE_PARENT/$CANDIDATE_ROOT_BASENAME" && pwd -P)
FT_GUI="$CANDIDATE_ROOT/$FT_GUI_RELATIVE"
FT_CLI="$CANDIDATE_ROOT/$FT_CLI_RELATIVE"
FT_MUX_SERVER="$CANDIDATE_ROOT/$FT_MUX_SERVER_RELATIVE"
FT_PTY_GUARDIAN="$CANDIDATE_ROOT/$FT_PTY_GUARDIAN_RELATIVE"
for snapshot_binary in "$FT_GUI" "$FT_CLI" "$FT_MUX_SERVER" "$FT_PTY_GUARDIAN"; do
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
   [ "$(hash_file "$FT_CLI")" != "$CLI_SHA256" ] || \
   [ "$(hash_file "$FT_MUX_SERVER")" != "$MUX_SERVER_SHA256" ] || \
   [ "$(hash_file "$FT_PTY_GUARDIAN")" != "$PTY_GUARDIAN_SHA256" ]; then
    echo "private candidate snapshot bytes differ from the verified candidate; refusing process launch" >&2
    exit 2
fi

verify_execution_snapshot_integrity() {
    local phase="$1"
    if [ "$(hash_file "$CANDIDATE_MANIFEST")" != "$MANIFEST_SHA256_AFTER" ] || \
       [ "$(hash_file "$ATOMIC_MANIFEST_TOOL")" != "$ATOMIC_MANIFEST_TOOL_SHA256" ] || \
       [ "$(hash_file "$FT_GUI")" != "$GUI_SHA256" ] || \
       [ "$(hash_file "$FT_CLI")" != "$CLI_SHA256" ] || \
       [ "$(hash_file "$FT_MUX_SERVER")" != "$MUX_SERVER_SHA256" ] || \
       [ "$(hash_file "$FT_PTY_GUARDIAN")" != "$PTY_GUARDIAN_SHA256" ]; then
        echo "candidate snapshot identity changed during $phase" >&2
        return 1
    fi
    if [ -n "${CANDIDATE_IDENTITY_SHA256:-}" ] && \
       [ "$(hash_file "$LOG_DIR/candidate-identity.json")" != "$CANDIDATE_IDENTITY_SHA256" ]; then
        echo "candidate identity record changed during $phase" >&2
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
    # Storage paths are relative to the owned workspace's .ft directory.
    "FT_STORAGE_DB_PATH=frankenterm.db"
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
if [ "$GUI_E2E_MODE" = nonactivating ]; then
    GUI_HERMETIC_ENV+=("FRANKENTERM_NATIVE_E2E_NONACTIVATING=1")
fi
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
        PRIVATE_MUX_SOCKET_VIOLATION=1
        echo "private watcher mux socket unexpectedly exists; refusing process launch: $PRIVATE_MUX_SOCKET_PATH" >&2
        return 1
    fi
}

# Prove the two non-interactive shipped roles are launchable without granting
# either one daemon, PTY, or ambient mux authority. `--version` is handled by
# each role's clap parser before service startup; Python supplies the finite
# wall-clock/process-group boundary that shell `timeout` cannot portably offer
# on macOS.
bounded_version_probe() {
    local binary="$1"
    local label="$2"
    local stdout_path="$LOG_DIR/$label-version.stdout"
    local stderr_path="$LOG_DIR/$label-version.stderr"
    "$ENV_BIN" -i \
        "PATH=$PREFLIGHT_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$HERMETIC_HOME" \
        "TMPDIR=$HERMETIC_TMPDIR" \
        PYTHONNOUSERSITE=1 \
        "$PYTHON3_BIN" - \
        "$binary" "$stdout_path" "$stderr_path" "$WORKSPACE_DIR" \
        "$SANITIZED_PATH" "$HERMETIC_HOME" "$HERMETIC_TMPDIR" \
        "$PRIVATE_MUX_SOCKET_PATH" <<'PY'
import os
import signal
import subprocess
import sys

(
    binary,
    stdout_path,
    stderr_path,
    workspace,
    path,
    home,
    tmpdir,
    private_mux_socket,
) = sys.argv[1:]
environment = {
    "PATH": path,
    "LANG": "C",
    "HOME": home,
    "TMPDIR": tmpdir,
    "FRANKENTERM_UNIX_SOCKET": private_mux_socket,
    "WEZTERM_UNIX_SOCKET": private_mux_socket,
}
try:
    with open(stdout_path, "wb") as stdout, open(stderr_path, "wb") as stderr:
        child = subprocess.Popen(
            [binary, "--version"],
            cwd=workspace,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            returncode = child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            raise SystemExit("version probe exceeded its 10-second deadline")
except OSError as error:
    raise SystemExit(f"version probe launch failed: {error}") from error
if returncode != 0:
    raise SystemExit(f"version probe exited with status {returncode}")
if not os.path.getsize(stdout_path):
    raise SystemExit("version probe emitted no stdout identity")
PY
}

# Invoked through the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
    local body_status=$?
    local cleanup_failed=0
    local settlement_failed=0
    local integrity_failed=0
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
        settlement_failed=1
    fi
    if [ -n "${WATCH_PID:-}" ] && ! stop_child "$WATCH_PID" "watch"; then
        printf '%s\n' "[cleanup] watch child settlement failed for pid $WATCH_PID" >&2
        cleanup_failed=1
        settlement_failed=1
    fi
    if ! settle_all_harness_jobs; then
        printf '%s\n' '[cleanup] one or more direct harness jobs escaped settlement' >&2
        cleanup_failed=1
        settlement_failed=1
    fi
    if ! verify_execution_snapshot_integrity after-child-settlement; then
        printf '%s\n' '[cleanup] candidate snapshot post-execution verification failed' >&2
        cleanup_failed=1
        integrity_failed=1
    fi
    if ! assert_private_mux_socket_absent; then
        printf '%s\n' '[cleanup] private mux socket appeared during native readiness execution' >&2
        cleanup_failed=1
    fi
    printf '%s\n' "[cleanup] Harness-owned child settlement attempt completed" >&2
    printf '%s\n' "[cleanup] Logs in $LOG_DIR" >&2
    local final_status=$body_status
    if [ "$final_status" -eq 0 ] && [ "$cleanup_failed" -ne 0 ]; then
        final_status=1
    fi
    if ! write_terminal_result \
        "$body_status" "$final_status" "$settlement_failed" "$integrity_failed"; then
        printf '%s\n' '[cleanup] failed to write terminal E2E result record' >&2
        if [ "$final_status" -eq 0 ]; then
            final_status=1
        fi
    fi
    exit "$final_status"
}

harness_job_is_active() {
    local pid="$1"
    local job_pid
    while IFS= builtin read -r job_pid; do
        [ "$job_pid" = "$pid" ] && return 0
    done < <(builtin jobs -pr)
    while IFS= builtin read -r job_pid; do
        [ "$job_pid" = "$pid" ] && return 0
    done < <(builtin jobs -ps)
    return 1
}

wait_child_bounded() {
    local pid="$1"
    local attempts=50
    while [ "$attempts" -gt 0 ]; do
        if ! harness_job_is_active "$pid"; then
            builtin wait "$pid" 2>/dev/null || true
            return 0
        fi
        attempts=$((attempts - 1))
        "$SLEEP_BIN" 0.1
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
    if ! harness_job_is_active "$pid"; then
        builtin wait "$pid" 2>/dev/null || true
        return 0
    fi
    # Job-table membership is the authority check immediately before every
    # numeric signal. It prevents a reaped/reused PID from targeting an
    # unrelated process; the remaining check-to-signal race is bounded by
    # Bash 3's process model and cannot be eliminated without pidfds.
    builtin kill -TERM "$pid" 2>/dev/null || true
    if wait_child_bounded "$pid"; then
        return 0
    fi
    echo "[cleanup] $label pid $pid missed TERM deadline; escalating exact child to KILL" >&2
    if ! harness_job_is_active "$pid"; then
        builtin wait "$pid" 2>/dev/null || true
        return 0
    fi
    builtin kill -KILL "$pid" 2>/dev/null || true
    wait_child_bounded "$pid"
}

# Called indirectly by cleanup through the EXIT trap.
# shellcheck disable=SC2329
settle_all_harness_jobs() {
    local snapshot_path="$LOG_DIR/cleanup-job-snapshot.txt"
    local job_pid
    local failed=0
    # Run `jobs` in this shell, not command substitution: its job table is the
    # authority surface. Sourcing is forbidden and the harness creates no
    # unrelated background jobs, so every active direct job is in scope. This
    # closes the signal window between `(...) &` and recording `$!`.
    if ! {
        builtin jobs -pr 2>/dev/null &&
        builtin jobs -ps 2>/dev/null
    } >"$snapshot_path"; then
        echo "[cleanup] cannot snapshot the harness job table" >&2
        return 1
    fi
    if [ ! -f "$snapshot_path" ] || [ -L "$snapshot_path" ] || [ ! -r "$snapshot_path" ]; then
        echo "[cleanup] harness job-table snapshot is not a readable regular file" >&2
        return 1
    fi
    while IFS= builtin read -r job_pid; do
        case "$job_pid" in
            ''|*[!0-9]*) continue ;;
        esac
        if ! stop_child "$job_pid" "direct harness job"; then
            failed=1
        fi
    done <"$snapshot_path" || return 1
    [ "$failed" -eq 0 ]
}

log_line_contains_both() {
    local log_path="$1"
    local first="$2"
    local second="$3"
    local line
    [ -f "$log_path" ] || return 1
    while IFS= read -r line; do
        if [[ $line == *"$first"* && $line == *"$second"* ]]; then
            return 0
        fi
    done <"$log_path"
    return 1
}

log_line_contains() {
    local log_path="$1"
    local needle="$2"
    local line
    [ -f "$log_path" ] || return 1
    while IFS= read -r line; do
        if [[ $line == *"$needle"* ]]; then
            return 0
        fi
    done <"$log_path"
    return 1
}

count_log_lines_containing() {
    local log_path="$1"
    local needle="$2"
    local count=0
    local line
    if [ -f "$log_path" ]; then
        while IFS= read -r line; do
            if [[ $line == *"$needle"* ]]; then
                count=$((count + 1))
            fi
        done <"$log_path"
    fi
    printf '%s\n' "$count"
}

wait_for_watch_bind() {
    local attempts=600
    while [ "$attempts" -gt 0 ]; do
        if log_line_contains_both \
            "$LOG_DIR/watch-stderr.log" \
            "Failed to bind native event socket, falling back to polling only" \
            "$SOCKET_PATH"; then
            return 2
        fi
        if log_line_contains_both \
            "$LOG_DIR/watch-stderr.log" \
            "Native event listener bound — waiting for GUI connections" \
            "$SOCKET_PATH"; then
            if harness_job_is_active "$WATCH_PID"; then
                return 0
            fi
            builtin wait "$WATCH_PID" 2>/dev/null || true
            WATCH_PID=''
            return 1
        fi
        if ! harness_job_is_active "$WATCH_PID"; then
            builtin wait "$WATCH_PID" 2>/dev/null || true
            WATCH_PID=''
            return 1
        fi
        attempts=$((attempts - 1))
        "$SLEEP_BIN" 0.1
    done
    return 3
}

wait_for_bridge_handshake() {
    local attempts=600
    local gui_connected_marker="Native event bridge: authenticated socket connected at $SOCKET_PATH"
    while [ "$attempts" -gt 0 ]; do
        if [ "$GUI_E2E_MODE" = nonactivating ]; then
            local observed_frontmost_pid
            observed_frontmost_pid=$(frontmost_application_pid) || return 4
            [ "$observed_frontmost_pid" = "$FRONTMOST_PID_BEFORE" ] || return 4
        fi
        if ! harness_job_is_active "$WATCH_PID"; then
            builtin wait "$WATCH_PID" 2>/dev/null || true
            WATCH_PID=''
            return 1
        fi
        if ! harness_job_is_active "$GUI_PID"; then
            builtin wait "$GUI_PID" 2>/dev/null || true
            GUI_PID=''
            return 2
        fi
        if log_line_contains "$LOG_DIR/gui-stderr.log" "$gui_connected_marker" && \
           log_line_contains "$LOG_DIR/watch-stderr.log" \
               "native event connection accepted (cx path)" && \
           log_line_contains "$LOG_DIR/watch-stderr.log" \
               "native event protocol hello received"; then
            return 0
        fi
        attempts=$((attempts - 1))
        "$SLEEP_BIN" 0.1
    done
    return 3
}

# Called indirectly by cleanup through the EXIT trap.
# shellcheck disable=SC2329
write_terminal_result() {
    local body_status="$1"
    local final_status="$2"
    local settlement_failed="$3"
    local integrity_failed="$4"
    "$ENV_BIN" -i \
        "PATH=$PREFLIGHT_PATH" \
        "LANG=${LANG:-C}" \
        "HOME=$LOG_DIR" \
        "TMPDIR=$LOG_DIR" \
        PYTHONNOUSERSITE=1 \
        "$PYTHON3_BIN" - \
        "$LOG_DIR/e2e-result.json" \
        "$body_status" \
        "$final_status" \
        "$settlement_failed" \
        "$integrity_failed" \
        "$PASS" \
        "$FAIL" \
        "$WATCH_LAUNCHED" \
        "$GUI_LAUNCHED" \
        "$FRANKENTERM_CANDIDATE_SHA" \
        "$FRANKENTERM_BUILD_PROFILE" \
        "$CANDIDATE_IDENTITY_SHA256" \
        "$MANIFEST_SHA256_AFTER" \
        "$CLI_SHA256" \
        "$GUI_SHA256" \
        "$MUX_SERVER_SHA256" \
        "$PTY_GUARDIAN_SHA256" \
        "$MUX_VERSION_PROBED" \
        "$GUARDIAN_VERSION_PROBED" \
        "$PRIVATE_MUX_SOCKET_VIOLATION" <<'PY'
import json
import sys
from pathlib import Path

(
    output_path,
    body_status,
    final_status,
    settlement_failed,
    integrity_failed,
    checks_passed,
    checks_failed,
    watch_launched,
    gui_launched,
    source_revision,
    profile,
    candidate_identity_sha256,
    manifest_sha256,
    cli_sha256,
    gui_sha256,
    mux_server_sha256,
    pty_guardian_sha256,
    mux_version_probed,
    guardian_version_probed,
    private_mux_socket_violation,
) = sys.argv[1:]
final_code = int(final_status)
record = {
    "schema_version": "ft.native_event_e2e_result.v1",
    "authority_scope": (
        "four_role_identity_and_bounded_launch_from_private_verified_bundle_copy; "
        "native_bridge_runtime_lifecycle; "
        "excludes LaunchServices, Gatekeeper, notarization, installation, latency, and soak"
    ),
    "e2e_result": "passed" if final_code == 0 else "failed",
    "candidate_identity": {
        "path": "candidate-identity.json",
        "sha256": candidate_identity_sha256 or None,
    },
    "source_revision": source_revision,
    "profile": profile,
    "component_digests": {
        "component_manifest": manifest_sha256,
        "ft": cli_sha256,
        "frankenterm-gui": gui_sha256,
        "frankenterm-mux-server": mux_server_sha256,
        "frankenterm-pty-guardian": pty_guardian_sha256,
    },
    "body_exit_status": int(body_status),
    "final_exit_status": final_code,
    "checks": {"passed": int(checks_passed), "failed": int(checks_failed)},
    "children_launched": {
        "watch": bool(int(watch_launched)),
        "gui": bool(int(gui_launched)),
    },
    "role_probes": {
        "frankenterm-mux-server--version": bool(int(mux_version_probed)),
        "frankenterm-pty-guardian--version": bool(int(guardian_version_probed)),
    },
    "private_mux_socket_absent_after_execution": not bool(
        int(private_mux_socket_violation)
    ),
    "child_settlement": "failed" if int(settlement_failed) else "passed",
    "post_execution_candidate_integrity": (
        "failed" if int(integrity_failed) else "passed"
    ),
}
Path(output_path).write_text(
    json.dumps(record, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
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

"$ENV_BIN" -i \
    "PATH=$PREFLIGHT_PATH" \
    "LANG=${LANG:-C}" \
    "HOME=$LOG_DIR" \
    "TMPDIR=$LOG_DIR" \
    PYTHONNOUSERSITE=1 \
    "$PYTHON3_BIN" - \
    "$LOG_DIR/candidate-identity.json" \
    "$FRANKENTERM_CANDIDATE_SHA" \
    "$FRANKENTERM_BUILD_PROFILE" \
    "$BASH_BIN" \
    "$BASH_VERSION" \
    "$PYTHON3_BIN" \
    "$PYTHON3_VERSION" \
    "$DECLARED_CANDIDATE_MANIFEST" \
    "$CANDIDATE_MANIFEST" \
    "$MANIFEST_SHA256_AFTER" \
    "$DECLARED_ATOMIC_MANIFEST_TOOL" \
    "$ATOMIC_MANIFEST_TOOL" \
    "$ATOMIC_MANIFEST_TOOL_SHA256" \
    "$DECLARED_CANDIDATE_ROOT" \
    "$CANDIDATE_ROOT" \
    "$DECLARED_FT_CLI" \
    "$FT_CLI" \
    "$CLI_SHA256" \
    "$DECLARED_FT_GUI" \
    "$FT_GUI" \
    "$GUI_SHA256" \
    "$DECLARED_FT_MUX_SERVER" \
    "$FT_MUX_SERVER" \
    "$MUX_SERVER_SHA256" \
    "$DECLARED_FT_PTY_GUARDIAN" \
    "$FT_PTY_GUARDIAN" \
    "$PTY_GUARDIAN_SHA256" <<'PY'
import json
import sys
from pathlib import Path

(
    output_path,
    source_revision,
    profile,
    bash_path,
    bash_version,
    python_path,
    python_version,
    declared_manifest,
    execution_manifest,
    manifest_sha256,
    declared_verifier,
    execution_verifier,
    verifier_sha256,
    declared_root,
    execution_root,
    declared_cli,
    execution_cli,
    cli_sha256,
    declared_gui,
    execution_gui,
    gui_sha256,
    declared_mux_server,
    execution_mux_server,
    mux_server_sha256,
    declared_pty_guardian,
    execution_pty_guardian,
    pty_guardian_sha256,
) = sys.argv[1:]
receipt = {
    "schema_version": "ft.native_event_e2e_candidate_identity.v1",
    "authority_scope": "candidate_identity_only",
    "e2e_result": "not_proven",
    "source_revision": source_revision,
    "profile": profile,
    "harness_runtime": {
        "bash": {"path": bash_path, "version": bash_version},
        "python": {"path": python_path, "version": python_version},
    },
    "component_manifest": {
        "declared_path": declared_manifest,
        "execution_path": execution_manifest,
        "sha256": manifest_sha256,
    },
    "verifier": {
        "declared_path": declared_verifier,
        "execution_path": execution_verifier,
        "sha256": verifier_sha256,
    },
    "candidate_root": {
        "declared_path": declared_root,
        "execution_path": execution_root,
    },
    "components": {
        "ft": {
            "declared_path": declared_cli,
            "execution_path": execution_cli,
            "sha256": cli_sha256,
        },
        "frankenterm-gui": {
            "declared_path": declared_gui,
            "execution_path": execution_gui,
            "sha256": gui_sha256,
        },
        "frankenterm-mux-server": {
            "declared_path": declared_mux_server,
            "execution_path": execution_mux_server,
            "sha256": mux_server_sha256,
        },
        "frankenterm-pty-guardian": {
            "declared_path": declared_pty_guardian,
            "execution_path": execution_pty_guardian,
            "sha256": pty_guardian_sha256,
        },
    },
}
Path(output_path).write_text(
    json.dumps(receipt, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
CANDIDATE_IDENTITY_SHA256=$(hash_file "$LOG_DIR/candidate-identity.json")

# Step 0: Launch the remaining two shipped roles only through their bounded,
# side-effect-free version parsers. This proves the exact manifest-bound mux
# and guardian bytes are executable without starting either service.
echo "[step 0] Probing mux-server and PTY-guardian identities..."
if bounded_version_probe "$FT_MUX_SERVER" mux-server; then
    MUX_VERSION_PROBED=1
    check "frankenterm-mux-server bounded version probe" "pass"
else
    check "frankenterm-mux-server bounded version probe" "fail"
    exit 1
fi
assert_private_mux_socket_absent
if bounded_version_probe "$FT_PTY_GUARDIAN" pty-guardian; then
    GUARDIAN_VERSION_PROBED=1
    check "frankenterm-pty-guardian bounded version probe" "pass"
else
    check "frankenterm-pty-guardian bounded version probe" "fail"
    exit 1
fi
assert_private_mux_socket_absent

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
    exec "$ENV_BIN" -i "${BASE_HERMETIC_ENV[@]}" \
        RUST_LOG=info,frankenterm_core::native_events=debug \
        WEZTERM_FT_SOCKET="$SOCKET_PATH" \
        "$FT_CLI" --config "$CONFIG_PATH" --workspace "$WORKSPACE_DIR" watch --foreground
) >"$LOG_DIR/watch-stdout.log" 2>"$LOG_DIR/watch-stderr.log" &
WATCH_PID=$!
WATCH_LAUNCHED=1
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
if [ "$GUI_E2E_MODE" = nonactivating ]; then
    FRONTMOST_PID_BEFORE=$(frontmost_application_pid) || {
        echo "could not resolve the frontmost application immediately before GUI launch" >&2
        exit 2
    }
fi
(
    cd "$WORKSPACE_DIR"
    exec "$ENV_BIN" -i "${GUI_HERMETIC_ENV[@]}" \
        RUST_LOG=info \
        WEZTERM_FT_SOCKET="$SOCKET_PATH" \
        "$FT_GUI" --skip-config --config check_for_updates=false \
        start --always-new-process --no-auto-connect -- /bin/cat
) >"$LOG_DIR/gui-stdout.log" 2>"$LOG_DIR/gui-stderr.log" &
GUI_PID=$!
GUI_LAUNCHED=1
if wait_for_bridge_handshake; then
    check "frankenterm-gui started" "pass"
else
    bridge_status=$?
    check "frankenterm-gui started" "fail"
    case "$bridge_status" in
        1) echo "ft watch exited while waiting for the authenticated GUI handshake" >&2 ;;
        2) echo "GUI exited before completing the authenticated native-event handshake" >&2 ;;
        3) echo "GUI/server handshake did not complete before the bounded deadline" >&2 ;;
        4) echo "frontmost application changed during the non-activating GUI smoke" >&2 ;;
        *) echo "unexpected bridge readiness failure: $bridge_status" >&2 ;;
    esac
    echo "Check $LOG_DIR/gui-stderr.log and $LOG_DIR/watch-stderr.log" >&2
    exit 1
fi
assert_private_mux_socket_absent

if [ "$GUI_E2E_MODE" = nonactivating ]; then
    FRONTMOST_PID_AFTER=$(frontmost_application_pid) || {
        check "native GUI preserved frontmost application" "fail"
        echo "could not resolve the frontmost application after GUI launch" >&2
        exit 1
    }
    if [ "$FRONTMOST_PID_AFTER" = "$FRONTMOST_PID_BEFORE" ]; then
        check "native GUI preserved frontmost application" "pass"
    else
        check "native GUI preserved frontmost application" "fail"
        echo "frontmost application changed from pid $FRONTMOST_PID_BEFORE to $FRONTMOST_PID_AFTER" >&2
        exit 1
    fi
fi

# Step 3: Check that the authenticated native event bridge connected.
if log_line_contains \
    "$LOG_DIR/gui-stderr.log" \
    "Native event bridge: authenticated socket connected at $SOCKET_PATH"; then
    check "GUI connected to native event socket" "pass"
else
    check "GUI connected to native event socket" "fail"
fi

# Step 4b: Prove the server, not just the GUI client, accepted the connection
# and decoded the GUI's protocol Hello frame.
if log_line_contains "$LOG_DIR/watch-stderr.log" \
       "native event connection accepted (cx path)" && \
   log_line_contains "$LOG_DIR/watch-stderr.log" \
       "native event protocol hello received"; then
    check "ft watch authenticated connection and decoded Hello" "pass"
else
    check "ft watch authenticated connection and decoded Hello" "fail"
fi

# Step 5: Stop the harness-owned GUI and verify ft watch stays alive.
echo "[step 5] Stopping harness-owned GUI, checking server-observed disconnect..."
if [ -z "${GUI_PID:-}" ] || ! harness_job_is_active "$GUI_PID"; then
    if [ -n "${GUI_PID:-}" ]; then
        builtin wait "$GUI_PID" 2>/dev/null || true
    fi
    GUI_PID=''
    check "GUI remained live until intentional harness stop" "fail"
    echo "GUI exited before the intentional lifecycle stop" >&2
    exit 1
fi
check "GUI remained live until intentional harness stop" "pass"
accepted_count_before=$(count_log_lines_containing \
    "$LOG_DIR/watch-stderr.log" \
    "native event connection accepted (cx path)")
disconnect_count_before=$(count_log_lines_containing \
    "$LOG_DIR/watch-stderr.log" \
    "native event connection closed (cx path)")
if [ "$accepted_count_before" -gt "$disconnect_count_before" ]; then
    check "authenticated bridge handler outstanding before GUI stop" "pass"
else
    check "authenticated bridge handler outstanding before GUI stop" "fail"
    echo "no outstanding authenticated handler remained to correlate with GUI stop" >&2
    exit 1
fi
if stop_child "$GUI_PID" "GUI"; then
    check "harness-owned GUI stopped within bounded cleanup" "pass"
    unset GUI_PID
else
    check "harness-owned GUI stopped within bounded cleanup" "fail"
    echo "GUI did not stop cleanly; retaining GUI_PID for EXIT cleanup" >&2
    exit 1
fi

disconnect_seen=false
disconnect_attempts=40
while [ "$disconnect_attempts" -gt 0 ]; do
    disconnect_count_after=$(count_log_lines_containing \
        "$LOG_DIR/watch-stderr.log" \
        "native event connection closed (cx path)")
    if [ "$disconnect_count_after" -ge "$accepted_count_before" ]; then
        disconnect_seen=true
        break
    fi
    disconnect_attempts=$((disconnect_attempts - 1))
    "$SLEEP_BIN" 0.1
done
if [ "$disconnect_seen" = true ]; then
    check "ft watch observed GUI connection close" "pass"
else
    check "ft watch observed GUI connection close" "fail"
fi

if harness_job_is_active "$WATCH_PID"; then
    check "ft watch survived GUI disconnect" "pass"
else
    builtin wait "$WATCH_PID" 2>/dev/null || true
    WATCH_PID=''
    check "ft watch survived GUI disconnect" "fail"
fi
assert_private_mux_socket_absent

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
