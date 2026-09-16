#!/usr/bin/env bash
set -euo pipefail

# create-macos-bundle.sh — Build FrankenTerm.app bundle from source
#
# Builds frankenterm-gui, frankenterm-mux-server, frankenterm-pty-guardian,
# and ft binaries, then
# packages them into a macOS .app bundle with the FrankenTerm icon and
# Info.plist.
#
# No dependency on a pre-installed WezTerm.app.
#
# Usage:
#   ./scripts/create-macos-bundle.sh               # build everything + bundle
#   ./scripts/create-macos-bundle.sh --skip-build # bundle only (uses existing binaries)
#   ./scripts/create-macos-bundle.sh --output /path/to/dir  # custom output directory
#   ./scripts/create-macos-bundle.sh --target aarch64-apple-darwin
#
# Safety:
#   Refuses to overwrite an existing FrankenTerm.app bundle. Use a fresh
#   output directory or remove the prior bundle manually.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

APP_NAME="FrankenTerm"
BUNDLE_ID="com.dicklesworthstone.frankenterm"
RCH_BIN="${RCH_BIN:-rch}"

SKIP_BUILD=false
OUTPUT_DIR="$PROJECT_ROOT"
TARGET_TRIPLE="${FT_ATOMIC_BUILD_TARGET:-}"
BUILD_PROFILE="release-interactive"
FEATURE_CONTRACT="application-family-gui-ft-mux-server-pty-guardian-default-features-v1"
BROWSER_RUNTIME_ROOT=""
BROWSER_RUNTIME_MANIFEST=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=true; shift ;;
        --output) OUTPUT_DIR="$2"; shift 2 ;;
        --target) TARGET_TRIPLE="$2"; shift 2 ;;
        --browser-runtime-root) BROWSER_RUNTIME_ROOT="$2"; shift 2 ;;
        --browser-runtime-manifest) BROWSER_RUNTIME_MANIFEST="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--skip-build] [--output DIR] [--target TRIPLE] --browser-runtime-root DIR --browser-runtime-manifest FILE"
            echo "  --skip-build  Skip cargo build, use existing binaries"
            echo "  --output DIR  Output directory for .app bundle (default: project root)"
            echo "  --target      Exact target triple embedded in every packaged executable"
            echo "  --browser-runtime-root      Exact preassembled Node/Playwright/Chromium component root"
            echo "  --browser-runtime-manifest  Detached atomic manifest for that component root"
            echo "                Existing FrankenTerm.app bundles are not overwritten."
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$TARGET_TRIPLE" ]]; then
    case "$(uname -s):$(uname -m)" in
        Darwin:arm64) TARGET_TRIPLE="aarch64-apple-darwin" ;;
        Darwin:x86_64) TARGET_TRIPLE="x86_64-apple-darwin" ;;
        *)
            echo "Error: cannot infer a macOS target triple on this host"
            echo "Pass --target with the exact target embedded in the build artifacts."
            exit 1
            ;;
    esac
fi
case "$TARGET_TRIPLE" in
    aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *)
        echo "Error: FrankenTerm.app requires a macOS target, got '$TARGET_TRIPLE'"
        exit 1
        ;;
esac

if [[ -n "${DSR_SOURCE_REPOSITORY:-}" && -z "${DSR_RELEASE_GIT_SHA:-}" ]]; then
    echo "Error: DSR_SOURCE_REPOSITORY requires DSR_RELEASE_GIT_SHA" >&2
    exit 1
fi
if [[ -n "${DSR_RELEASE_GIT_SHA:-}" && ( -n "${DSR_SOURCE_REPOSITORY:-}" \
    || "$(pwd -P)" != "$(cd "$PROJECT_ROOT" && pwd -P)" ) ]]; then
    # DSR_SOURCE_REPOSITORY is Git-object authority, never an asset directory.
    # It permits this script to run from the immutable archive itself. Keep
    # the canonical-script caller's repository default for existing callers.
    DSR_SOURCE_ROOT="$(pwd -P)"
    if [[ -z "${CARGO_TARGET_DIR:-}" || ! -d "$CARGO_TARGET_DIR" || -e "$DSR_SOURCE_ROOT/.git" \
        || "$DSR_SOURCE_ROOT" != "$(cd "$CARGO_TARGET_DIR/.." && pwd -P)/source" ]]; then
        echo "Error: DSR packaging requires its source archive and sibling Cargo target directory" >&2
        exit 1
    fi
    # Bootstrap the verifier from the requested commit, not unverified archive
    # bytes or the repository's possibly newer working tree. Bound Git output
    # by its immutable blob size before loading executable verifier bytes.
    python3 - "${DSR_SOURCE_REPOSITORY:-$PROJECT_ROOT}" "$DSR_RELEASE_GIT_SHA" "$DSR_SOURCE_ROOT" <<'PY'
import os
import re
import subprocess
import sys

repository, revision, root = sys.argv[1:]
if not re.fullmatch(r"[0-9a-f]{40}", revision):
    raise SystemExit("invalid DSR source revision")
environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
command = ["git", "--no-replace-objects", "-C", repository]
try:
    def git(*arguments):
        return subprocess.check_output(command + list(arguments), env=environment, timeout=30)

    if git("rev-parse", "--verify", revision + "^{commit}").decode().strip() != revision:
        raise SystemExit("DSR source revision does not name the requested commit")
    blob = git("rev-parse", "--verify", revision + ":scripts/atomic-component-manifest.sh").decode().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", blob) or git("cat-file", "-t", blob) != b"blob\n":
        raise SystemExit("DSR source verifier is not an immutable Git blob")
    size = int(git("cat-file", "-s", blob))
    if not 0 < size <= 1024 * 1024:
        raise SystemExit("DSR source verifier exceeds the byte limit")
    verifier = git("cat-file", "blob", blob)
    if len(verifier) != size:
        raise SystemExit("DSR source verifier size changed")
    subprocess.run(
        ["bash", "-s", "--", "verify-source", "--root", root,
         "--repository", repository, "--source-revision", revision],
        input=verifier, env=environment, check=True,
    )
except (OSError, ValueError, subprocess.SubprocessError) as error:
    raise SystemExit(f"DSR immutable source verification failed: {error}") from error
PY
    PROJECT_ROOT="$DSR_SOURCE_ROOT"
    SOURCE_REVISION="$DSR_RELEASE_GIT_SHA"
else
    SOURCE_REVISION=$(git -C "$PROJECT_ROOT" rev-parse HEAD)
    if [[ -n "${DSR_RELEASE_GIT_SHA:-}" && "$SOURCE_REVISION" != "$DSR_RELEASE_GIT_SHA" ]]; then
        echo "Error: checkout source differs from DSR_RELEASE_GIT_SHA" >&2
        exit 1
    fi
    if ! git -C "$PROJECT_ROOT" diff --quiet -- || ! git -C "$PROJECT_ROOT" diff --cached --quiet --; then
        echo "Error: tracked source changes are present; refusing to mint a commit-bound package identity"
        echo "Commit the intended source snapshot, then rebuild all components together."
        exit 1
    fi
fi
VERSION=$(grep -m1 '^version' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
if [[ ! "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Error: cannot resolve a full source revision for atomic packaging"
    exit 1
fi
ATOMIC_MANIFEST_TOOL="$PROJECT_ROOT/scripts/atomic-component-manifest.sh"
if [[ ! -f "$ATOMIC_MANIFEST_TOOL" ]]; then
    echo "Error: atomic component manifest tool not found at $ATOMIC_MANIFEST_TOOL"
    exit 1
fi
if [[ -z "$BROWSER_RUNTIME_ROOT" || -z "$BROWSER_RUNTIME_MANIFEST" ]]; then
    echo "Error: an exact browser runtime root and detached component manifest are required"
    echo "Pass --browser-runtime-root and --browser-runtime-manifest; ambient Playwright is forbidden."
    exit 1
fi
if [[ ! -d "$BROWSER_RUNTIME_ROOT" || ! -f "$BROWSER_RUNTIME_MANIFEST" ]]; then
    echo "Error: browser runtime root or manifest is unavailable"
    exit 1
fi
if ! bash "$ATOMIC_MANIFEST_TOOL" verify \
    --root "$BROWSER_RUNTIME_ROOT" \
    --manifest "$BROWSER_RUNTIME_MANIFEST"; then
    echo "Error: browser runtime component failed offline verification"
    exit 1
fi

BROWSER_RUNTIME_METADATA=()
while IFS= read -r browser_runtime_record; do
    BROWSER_RUNTIME_METADATA+=("$browser_runtime_record")
done < <(python3 - "$BROWSER_RUNTIME_MANIFEST" "$TARGET_TRIPLE" <<'PY'
import json
import sys

manifest_path, expected_target = sys.argv[1:]
with open(manifest_path, "rb") as handle:
    manifest = json.load(handle)
if manifest.get("identity", {}).get("target") != expected_target:
    raise SystemExit("browser runtime target does not match requested app target")
contracts = manifest.get("contracts", {})
required = [
    "browser.runtime.root",
    "browser.node.path",
    "browser.node.version",
    "browser.playwright.module-path",
    "browser.playwright.browsers-path",
    "browser.playwright.version",
    "browser.chromium.executable-path",
    "browser.chromium.revision",
    "browser.protocol.version",
    "browser.license.node-path",
    "browser.license.playwright-path",
    "browser.license.chromium-path",
    "browser.symlink-manifest.path",
    "browser.disk-budget.bytes",
]
for key in required:
    value = contracts.get(key)
    if not isinstance(value, str) or not value or "\n" in value or "\r" in value or "=" in key:
        raise SystemExit(f"browser runtime contract is missing or unsafe: {key}")
    print(f"{key}={value}")
manifest_id = manifest.get("manifest_id")
if not isinstance(manifest_id, str):
    raise SystemExit("browser runtime manifest identity is missing")
print(f"browser.component.source-manifest-id={manifest_id}")
PY
)
if [[ "${#BROWSER_RUNTIME_METADATA[@]}" -ne 15 ]]; then
    echo "Error: browser runtime contract extraction was incomplete"
    exit 1
fi

browser_contract_value() {
    local wanted="$1"
    local record
    for record in "${BROWSER_RUNTIME_METADATA[@]}"; do
        if [[ "$record" == "$wanted="* ]]; then
            printf '%s\n' "${record#*=}"
            return 0
        fi
    done
    return 1
}

BROWSER_RUNTIME_SOURCE_ROOT_REL=$(browser_contract_value browser.runtime.root)
BROWSER_NODE_PATH_REL=$(browser_contract_value browser.node.path)
BROWSER_NODE_VERSION=$(browser_contract_value browser.node.version)
BROWSER_PLAYWRIGHT_MODULE_REL=$(browser_contract_value browser.playwright.module-path)
BROWSER_PLAYWRIGHT_BROWSERS_REL=$(browser_contract_value browser.playwright.browsers-path)
BROWSER_PLAYWRIGHT_VERSION=$(browser_contract_value browser.playwright.version)
BROWSER_CHROMIUM_EXECUTABLE_REL=$(browser_contract_value browser.chromium.executable-path)
BROWSER_CHROMIUM_REVISION=$(browser_contract_value browser.chromium.revision)
BROWSER_PROTOCOL_VERSION=$(browser_contract_value browser.protocol.version)
BROWSER_NODE_LICENSE_REL=$(browser_contract_value browser.license.node-path)
BROWSER_PLAYWRIGHT_LICENSE_REL=$(browser_contract_value browser.license.playwright-path)
BROWSER_CHROMIUM_LICENSE_REL=$(browser_contract_value browser.license.chromium-path)
BROWSER_SYMLINK_MANIFEST_REL=$(browser_contract_value browser.symlink-manifest.path)
BROWSER_DISK_BUDGET_BYTES=$(browser_contract_value browser.disk-budget.bytes)
BROWSER_SOURCE_MANIFEST_ID=$(browser_contract_value browser.component.source-manifest-id)
PANIC_CONTRACT_TOOL="$PROJECT_ROOT/scripts/check-release-panic-contract.sh"
if [[ ! -f "$PANIC_CONTRACT_TOOL" ]]; then
    echo "Error: release panic-contract checker not found at $PANIC_CONTRACT_TOOL"
    exit 1
fi
if ! bash "$PANIC_CONTRACT_TOOL" --profiles-only; then
    echo "Error: Cargo release profiles do not satisfy the shipped panic contract"
    exit 1
fi
EXPECTED_BUILD_ID=$(bash "$ATOMIC_MANIFEST_TOOL" derive-build-id \
    --source-revision "$SOURCE_REVISION" \
    --version "$VERSION" \
    --target "$TARGET_TRIPLE" \
    --profile "$BUILD_PROFILE" \
    --feature-contract "$FEATURE_CONTRACT")
if [[ -n "${FT_ATOMIC_BUILD_IDENTITY:-}" && "$FT_ATOMIC_BUILD_IDENTITY" != "$EXPECTED_BUILD_ID" ]]; then
    echo "Error: supplied atomic build identity does not match this source/build contract"
    echo "Expected: $EXPECTED_BUILD_ID"
    echo "Supplied: $FT_ATOMIC_BUILD_IDENTITY"
    echo "Rebuild GUI, ft, mux-server, and PTY guardian together from this exact source snapshot."
    exit 1
fi
FT_ATOMIC_BUILD_IDENTITY="$EXPECTED_BUILD_ID"
export FT_ATOMIC_BUILD_IDENTITY

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
CARGO_TARGET_DIR_REL=""
CARGO_TARGET_DIR_IN_REPO=0

resolve_project_path_info() {
    python3 - "$PROJECT_ROOT" "$1" <<'PY'
import os
import sys

root = os.path.realpath(sys.argv[1])
value = sys.argv[2]
abs_path = os.path.realpath(value if os.path.isabs(value) else os.path.join(root, value))
rel_path = os.path.relpath(abs_path, root)
in_repo = rel_path == "." or (rel_path != ".." and not rel_path.startswith(f"..{os.sep}"))

print(abs_path)
print(rel_path)
print("1" if in_repo else "0")
PY
}

normalize_cargo_target_dir() {
    local -a info=()
    local record
    while IFS= read -r record; do
        info+=("$record")
    done < <(resolve_project_path_info "$CARGO_TARGET_DIR")
    if [[ "${#info[@]}" -ne 3 ]]; then
        echo "Error: failed to normalize CARGO_TARGET_DIR"
        return 1
    fi
    CARGO_TARGET_DIR="${info[0]}"
    CARGO_TARGET_DIR_REL="${info[1]}"
    CARGO_TARGET_DIR_IN_REPO="${info[2]}"
}

require_remote_safe_target_dir() {
    if [[ "$CARGO_TARGET_DIR_IN_REPO" == "1" ]]; then
        return 0
    fi

    echo "Error: CARGO_TARGET_DIR '$CARGO_TARGET_DIR' is outside project root '$PROJECT_ROOT'"
    echo "Use a repo-relative target dir (for example target or target/gui-bundle) when offloading via rch."
    return 1
}

resolve_rch_cmd() {
    if [[ "$RCH_BIN" == */* && -r "$RCH_BIN" ]]; then
        local shebang=""
        IFS= read -r shebang < "$RCH_BIN" || true
        case "$shebang" in
            '#!'*bash*|'#!'*sh)
                printf '%s\n' "/bin/bash"
                printf '%s\n' "$RCH_BIN"
                return 0
                ;;
        esac
    fi

    printf '%s\n' "$RCH_BIN"
}

run_rch() {
    local -a cmd=()
    local record
    while IFS= read -r record; do
        cmd+=("$record")
    done < <(resolve_rch_cmd)
    if [[ "${#cmd[@]}" -eq 0 ]]; then
        echo "Error: failed to resolve rch command"
        return 1
    fi
    "${cmd[@]}" "$@"
}

run_rch_bundle_build() {
    (
        cd "$PROJECT_ROOT"
        # RCH 1.0.62 makes source-content receipts mutually exclusive with the
        # clean-baseline mode; the committed base with no overlay is authoritative.
        RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
        run_rch --no-self-healing exec \
            --base "$SOURCE_REVISION" \
            --clean-overlay \
            --no-overlay \
            -- env \
            CARGO_TARGET_DIR="$CARGO_TARGET_DIR_REL" \
            FT_ATOMIC_BUILD_IDENTITY="$FT_ATOMIC_BUILD_IDENTITY" \
            FT_ATOMIC_BUILD_PROFILE="$BUILD_PROFILE" \
            cargo build --locked --profile "$BUILD_PROFILE" --target "$TARGET_TRIPLE" \
            --bin frankenterm-gui \
            --bin frankenterm-mux-server \
            --bin frankenterm-pty-guardian \
            --bin ft \
            --manifest-path Cargo.toml
    )
}

build_remote_bundle_with_diagnostics() {
    local preflight_log="$PROJECT_ROOT/target/e2e/gui-bootstrap/bundle-build.log"
    mkdir -p "$(dirname "$preflight_log")"
    : > "$preflight_log"

    # RCH deliberately rejects arbitrary `sh -lc` prerequisite probes
    # (RCH-E301). The exact fail-closed Cargo build is both the authoritative
    # prerequisite check and artifact producer: native build scripts diagnose
    # missing metadata, while success proves all four processes linked.
    if run_rch_bundle_build > >(tee -a "$preflight_log") 2> >(tee -a "$preflight_log" >&2); then
        return 0
    fi

    echo "Error: strict-remote GUI/mux/CLI bundle build failed."
    echo "On Linux workers, verify pkg-config plus x11, xcb-image, and xkbcommon-x11 development metadata."
    echo "See $preflight_log for the authoritative remote Cargo output."
    return 1
}

require_rch() {
    if [[ "$RCH_BIN" == */* ]]; then
        [[ -x "$RCH_BIN" ]]
        return
    fi
    command -v "$RCH_BIN" >/dev/null 2>&1
}

probe_rch_workers() {
    local probe_json
    probe_json="$(RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 run_rch --no-self-healing workers probe --json --all)"
    python3 - "$probe_json" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
data = payload.get("data", [])
workers = data.get("results", []) if isinstance(data, dict) else data
if not isinstance(workers, list):
    raise SystemExit("rch worker probe returned an invalid data shape")
for worker in workers:
    if not isinstance(worker, dict):
        raise SystemExit("rch worker probe returned an invalid worker record")
    status = str(worker.get("status", "")).strip().lower()
    if status and not status.endswith("_failed") and status not in {
        "connection_failed",
        "error",
        "failed",
        "unreachable",
    }:
        sys.exit(0)

sys.exit(1)
PY
}

normalize_cargo_target_dir

# --- Build from source ---
if [ "$SKIP_BUILD" = false ]; then
    if ! require_rch; then
        echo "Error: rch not found at '$RCH_BIN'"
        exit 1
    fi
    if ! require_remote_safe_target_dir; then
        exit 1
    fi
    if ! probe_rch_workers; then
        echo "Error: no reachable RCH workers detected; refusing local cargo fallback"
        exit 1
    fi
    echo "Building frankenterm-gui, frankenterm-mux-server, PTY guardian, and bundled ft via rch ($BUILD_PROFILE, panic=unwind)..."
    if ! build_remote_bundle_with_diagnostics; then
        exit 1
    fi
fi

# --- Locate binaries ---
BINARY_DIR="$CARGO_TARGET_DIR/$TARGET_TRIPLE/$BUILD_PROFILE"
GUI_BINARY="$BINARY_DIR/frankenterm-gui"
MUX_SERVER_BINARY="$BINARY_DIR/frankenterm-mux-server"
GUARDIAN_BINARY="$BINARY_DIR/frankenterm-pty-guardian"
FT_BINARY="$BINARY_DIR/ft"

if [ ! -f "$GUI_BINARY" ]; then
    echo "Error: frankenterm-gui binary not found at $GUI_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi
if [ ! -f "$MUX_SERVER_BINARY" ]; then
    echo "Error: frankenterm-mux-server binary not found at $MUX_SERVER_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi
if [ ! -f "$GUARDIAN_BINARY" ]; then
    echo "Error: frankenterm-pty-guardian binary not found at $GUARDIAN_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi
if [ ! -f "$FT_BINARY" ]; then
    echo "Error: ft binary not found at $FT_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi

# --- Bundle build string ---
if [[ -n "${SOURCE_DATE_EPOCH:-}" ]]; then
    BUILD_STRING=$(python3 - "$SOURCE_DATE_EPOCH" <<'PY'
from datetime import datetime, timezone
import sys

try:
    epoch = int(sys.argv[1])
except ValueError as exc:
    raise SystemExit(f"SOURCE_DATE_EPOCH must be a non-negative integer: {exc}")
if epoch < 0:
    raise SystemExit("SOURCE_DATE_EPOCH must be a non-negative integer")
print(datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y%m%d.%H%M%S"))
PY
    )
else
    BUILD_STRING=$(date -u +%Y%m%d.%H%M%S)
fi

echo "Packaging $APP_NAME.app v$VERSION (build $BUILD_STRING)..."

# --- Bundle structure ---
APP_BUNDLE="$OUTPUT_DIR/$APP_NAME.app"
ATOMIC_MANIFEST="$OUTPUT_DIR/$APP_NAME.app.component-manifest.json"
if ! python3 - "$BROWSER_RUNTIME_ROOT" "$APP_BUNDLE" <<'PY'
import os
import sys

source = os.path.realpath(sys.argv[1])
destination = os.path.realpath(sys.argv[2])
try:
    common = os.path.commonpath((source, destination))
except ValueError as error:
    raise SystemExit(f"cannot compare browser runtime and app paths: {error}") from error
if common == source or common == destination:
    raise SystemExit(
        "browser runtime source and app destination must not contain one another: "
        f"source={source!r} destination={destination!r}"
    )
PY
then
    echo "Error: unsafe browser runtime/app destination relationship"
    echo "Choose a --browser-runtime-root and --output directory with disjoint trees."
    exit 1
fi
if [ -e "$APP_BUNDLE" ]; then
    echo "Error: app bundle already exists at $APP_BUNDLE"
    echo "Choose a fresh --output directory or remove the existing bundle manually."
    exit 1
fi
if [ -e "$ATOMIC_MANIFEST" ]; then
    echo "Error: atomic component manifest already exists at $ATOMIC_MANIFEST"
    echo "Choose a fresh --output directory. Existing authority files are never overwritten."
    exit 1
fi
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# Install the preassembled browser capability as one immutable tree. The
# detached source manifest is retained as provenance, while the final app
# manifest below re-hashes the post-signing bytes that runtime preflight uses.
BROWSER_RUNTIME_DEST="$APP_BUNDLE/Contents/Resources/browser-runtime"
BROWSER_SOURCE_MANIFEST_DEST="$APP_BUNDLE/Contents/Resources/browser-runtime.source-manifest.json"
mkdir -p "$BROWSER_RUNTIME_DEST"
# Archive streaming preserves the component's verified internal symlink
# topology without following those links or merging it with prior output.
COPYFILE_DISABLE=1 tar -C "$BROWSER_RUNTIME_ROOT" -cf - . \
    | COPYFILE_DISABLE=1 tar -C "$BROWSER_RUNTIME_DEST" -xf -
cp "$BROWSER_RUNTIME_MANIFEST" "$BROWSER_SOURCE_MANIFEST_DEST"
if ! bash "$ATOMIC_MANIFEST_TOOL" verify \
    --root "$BROWSER_RUNTIME_DEST" \
    --manifest "$BROWSER_SOURCE_MANIFEST_DEST"; then
    echo "Error: copied browser runtime differs from its verified source component"
    exit 1
fi

BROWSER_BUNDLE_PREFIX="Contents/Resources/browser-runtime"
prefix_browser_path() {
    printf '%s/%s\n' "$BROWSER_BUNDLE_PREFIX" "$1"
}
BROWSER_RUNTIME_BUNDLE_ROOT=$(prefix_browser_path "$BROWSER_RUNTIME_SOURCE_ROOT_REL")
BROWSER_NODE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_NODE_PATH_REL")
BROWSER_PLAYWRIGHT_MODULE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_PLAYWRIGHT_MODULE_REL")
BROWSER_PLAYWRIGHT_BROWSERS_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_PLAYWRIGHT_BROWSERS_REL")
BROWSER_CHROMIUM_EXECUTABLE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_CHROMIUM_EXECUTABLE_REL")
BROWSER_NODE_LICENSE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_NODE_LICENSE_REL")
BROWSER_PLAYWRIGHT_LICENSE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_PLAYWRIGHT_LICENSE_REL")
BROWSER_CHROMIUM_LICENSE_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_CHROMIUM_LICENSE_REL")
BROWSER_SYMLINK_MANIFEST_BUNDLE_PATH=$(prefix_browser_path "$BROWSER_SYMLINK_MANIFEST_REL")
BROWSER_SOURCE_MANIFEST_BUNDLE_PATH="Contents/Resources/browser-runtime.source-manifest.json"

# --- Copy binaries built from source ---
echo "Installing frankenterm-gui..."
cp "$GUI_BINARY" "$APP_BUNDLE/Contents/MacOS/frankenterm-gui"

echo "Installing frankenterm-mux-server..."
cp "$MUX_SERVER_BINARY" "$APP_BUNDLE/Contents/MacOS/frankenterm-mux-server"

echo "Installing frankenterm-pty-guardian..."
cp "$GUARDIAN_BINARY" "$APP_BUNDLE/Contents/MacOS/frankenterm-pty-guardian"

echo "Installing ft CLI..."
cp "$FT_BINARY" "$APP_BUNDLE/Contents/MacOS/ft"

# Resolve the native link closure before signing. Homebrew's Cairo install name
# otherwise makes an apparently complete app depend on the packaging host.
# Only fresh package copies are changed; Cargo artifacts and host libraries
# remain untouched. Unresolved/ambiguous loader forms fail closed.
python3 - "$APP_BUNDLE" "$BINARY_DIR" "$TARGET_TRIPLE" <<'PY_NATIVE_DYLIB_CLOSURE'
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def command(*args):
    return subprocess.check_output(args, text=True, timeout=60)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def system_library(name):
    return name.startswith(("/usr/lib/", "/System/Library/")) and ".." not in Path(name).parts


def macho(path, architecture):
    if command("lipo", "-archs", str(path)).split() != [architecture]:
        raise ValueError(f"native dependency has the wrong architecture: {path}")
    loads, identities, rpaths = [], [], []
    load_commands = {
        "LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB", "LC_REEXPORT_DYLIB",
        "LC_LOAD_UPWARD_DYLIB", "LC_LAZY_LOAD_DYLIB",
    }
    blocks = re.split(r"(?m)^Load command \d+\n", command("otool", "-l", str(path)))[1:]
    if not blocks:
        raise ValueError(f"native image has no readable load commands: {path}")
    for block in blocks:
        match = re.search(r"(?m)^\s*cmd (LC_\S+)$", block)
        if not match:
            raise ValueError(f"unreadable Mach-O load command: {path}")
        kind = match[1]
        if kind in load_commands or kind in {"LC_ID_DYLIB", "LC_RPATH"}:
            field = "path" if kind == "LC_RPATH" else "name"
            value = re.search(r"(?m)^\s*" + field + r" (.+) \(offset \d+\)$", block)
            if not value or any(ord(char) < 32 for char in value[1]):
                raise ValueError(f"unreadable native dependency path: {path}")
            (rpaths if kind == "LC_RPATH" else identities if kind == "LC_ID_DYLIB" else loads).append(value[1])
        elif "DYLIB" in kind:
            raise ValueError(f"unsupported native dependency command {kind}: {path}")
    if len(identities) > 1:
        raise ValueError(f"multiple native library identities: {path}")
    return loads, identities, rpaths


def dependency_path(name, source):
    if name.startswith("@loader_path/"):
        candidate = source.parent / name[len("@loader_path/"):]
    elif name.startswith("/"):
        candidate = Path(name)
    else:
        # In particular, do not guess @rpath from the packager's environment.
        raise ValueError(f"unresolved native dependency {name!r} in {source}")
    result = candidate.resolve(strict=True)
    if not result.is_file() or result.suffix != ".dylib":
        raise ValueError(f"native dependency is not a regular dylib: {result}")
    return result


def bundle_native_dependencies(bundle, binary_dir, architecture):
    frameworks = bundle / "Contents/Frameworks"
    metadata = bundle / "Contents/Resources/native-dependencies"
    roots = [binary_dir / name for name in (
        "frankenterm-gui", "frankenterm-mux-server", "frankenterm-pty-guardian", "ft",
    )]
    destinations = {path: bundle / "Contents/MacOS" / path.name for path in roots}
    pending, graph, names = list(roots), {}, {}
    # Inspect the whole graph before changing package bytes. Canonical paths
    # collapse Cellar/opt aliases and terminate cycles; names never pick a winner.
    while pending:
        source = pending.pop(0)
        if source in graph:
            continue
        if len(graph) >= 256:
            raise ValueError("native dependency closure exceeds 256 images")
        loads, identities, rpaths = macho(source, architecture)
        if source not in roots and not identities:
            raise ValueError(f"native dependency has no LC_ID_DYLIB: {source}")
        edges = {}
        for name in loads:
            if system_library(name):
                continue
            dependency = dependency_path(name, source)
            existing = names.setdefault(dependency.name.casefold(), dependency)
            if existing != dependency:
                raise ValueError(f"native dependency basename collision: {existing} and {dependency}")
            edges[name] = dependency
            if dependency not in destinations:
                destinations[dependency] = frameworks / dependency.name
                pending.append(dependency)
        graph[source] = (edges, identities, rpaths, digest(source), loads)

    frameworks.mkdir()
    metadata.mkdir()
    receipts = []
    for source in sorted(graph, key=str):
        edges, identities, rpaths, source_sha256, loads = graph[source]
        destination = destinations[source]
        if source not in roots:
            # Retain the actual package license notices and upstream provenance,
            # not just the rewritten dylib. Unknown package layouts need an
            # explicit packaging implementation rather than an unlicensed copy.
            package = next((parent for parent in source.parents
                            if (parent / "INSTALL_RECEIPT.json").is_file()), None)
            if package is None:
                raise ValueError(f"native dependency package provenance unavailable: {source}")
            notices = sorted(path for path in package.iterdir() if path.is_file()
                             and path.name.upper().startswith(("COPYING", "LICENSE", "LICENCE", "NOTICE")))
            if not notices:
                raise ValueError(f"native dependency license notices unavailable: {source}")
            notice_dir = metadata / source.name
            notice_dir.mkdir()
            for path in notices + [package / "INSTALL_RECEIPT.json"]:
                shutil.copyfile(path, notice_dir / path.name)
            if (package / "sbom.spdx.json").is_file():
                shutil.copyfile(package / "sbom.spdx.json", notice_dir / "sbom.spdx.json")
            with source.open("rb") as reader, destination.open("xb") as writer:
                shutil.copyfileobj(reader, writer)
            destination.chmod(0o755)
        if destination.is_symlink() or not destination.is_file() or digest(destination) != source_sha256:
            raise ValueError(f"package image differs from its source before relocation: {destination}")
        changes = []
        for old, dependency in sorted(edges.items()):
            relative = os.path.relpath(destinations[dependency], destination.parent)
            changes.extend(("-change", old, "@loader_path/" + relative))
        if identities:
            changes.extend(("-id", "@loader_path/" + destination.name))
        for rpath in sorted(set(rpaths)):
            changes.extend(("-delete_rpath", rpath))
        if changes:
            command("install_name_tool", *changes, str(destination))
        receipts.append({"source": str(source), "source_sha256": source_sha256,
                         "bundled_path": str(destination.relative_to(bundle))})

    for source, destination in destinations.items():
        loads, identities, rpaths = macho(destination, architecture)
        edges, original_ids, _, _, original_loads = graph[source]
        expected_loads = [
            "@loader_path/" + os.path.relpath(destinations[edges[name]], destination.parent)
            if name in edges else name for name in original_loads
        ]
        expected_ids = ["@loader_path/" + destination.name] if original_ids else []
        if loads != expected_loads or identities != expected_ids:
            raise ValueError(f"packaged native load commands differ from the planned closure: {destination}")
        if rpaths:
            raise ValueError(f"packaged native image retains a search path: {destination}")
        for name in loads + identities:
            if system_library(name):
                continue
            if not name.startswith("@loader_path/"):
                raise ValueError(f"packaged native image retains external dependency {name}: {destination}")
            resolved = dependency_path(name, destination)
            if resolved not in destinations.values():
                raise ValueError(f"packaged native dependency escapes the closure: {name}")
        if digest(source) != graph[source][3]:
            raise ValueError(f"native dependency source changed while packaging: {source}")
    with (metadata / "closure.json").open("x") as handle:
        json.dump({"schema": "ft.native-dylib-closure.v1", "images": receipts}, handle, indent=2, sort_keys=True)
        handle.write("\n")


if __name__ == "__main__":
    app, binaries, target = sys.argv[1:]
    try:
        bundle_native_dependencies(Path(app).resolve(), Path(binaries).resolve(),
                                   {"aarch64-apple-darwin": "arm64", "x86_64-apple-darwin": "x86_64"}[target])
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise SystemExit(f"native dependency packaging failed: {error}") from error
PY_NATIVE_DYLIB_CLOSURE

# --- Guard (GH #70): bundled defaults must be generic/local-only (no live
#     remote hosts, SSH keys, or proxy commands that auto-connect on first
#     launch). Fails the bundle if the defaults regress. ---
bash "$PROJECT_ROOT/scripts/check_bundled_default_config_generic.sh"

# --- Copy default config ---
DEFAULT_CONFIG="$PROJECT_ROOT/crates/frankenterm-gui/frankenterm.toml"
if [ ! -f "$DEFAULT_CONFIG" ]; then
    echo "Error: bundled default TOML config not found at $DEFAULT_CONFIG"
    exit 1
fi
cp "$DEFAULT_CONFIG" "$APP_BUNDLE/Contents/Resources/frankenterm.toml"

# --- Copy default GUI Lua config (loaded by frankenterm-gui when no user
#     ~/.frankenterm.lua / ~/.config/frankenterm/*.lua / ~/.wezterm.lua exists).
#     Resolved via the macOS-bundle fallback in
#     frankenterm/config/src/config.rs::Configuration::load (search for
#     "Last-resort fallback: bundled default config").
#     File is named frankenterm.lua (not wezterm.lua) to keep the bundled
#     defaults under the FrankenTerm namespace; the config loader checks
#     both names in that order.
DEFAULT_LUA="$PROJECT_ROOT/crates/frankenterm-gui/frankenterm.lua"
if [ ! -f "$DEFAULT_LUA" ]; then
    echo "Error: bundled default Lua config not found at $DEFAULT_LUA"
    exit 1
fi
cp "$DEFAULT_LUA" "$APP_BUNDLE/Contents/Resources/frankenterm.lua"

# --- Bundle the default Pragmasevka Nerd Font ---
FONT_PAYLOAD="$PROJECT_ROOT/crates/frankenterm/assets/Pragmasevka_NF.zip.zst"
FONT_DIR="$APP_BUNDLE/Contents/Resources/fonts"
if [ ! -f "$FONT_PAYLOAD" ]; then
    echo "Error: bundled Pragmasevka font payload not found at $FONT_PAYLOAD"
    exit 1
fi
mkdir -p "$FONT_DIR"
if ! command -v zstd >/dev/null 2>&1; then
    echo "Error: zstd is required to unpack the bundled Pragmasevka font payload"
    exit 1
fi
zstd -dc "$FONT_PAYLOAD" | /usr/bin/tar -xf - -C "$FONT_DIR"

# Only the repository-pinned payload may contribute bundled fonts. Pulling
# matching faces from the packaging host made output depend on ambient user
# state and could silently mix font versions into an otherwise coherent build.

# --- Copy FrankenTerm icon ---
ICNS="$PROJECT_ROOT/assets/macos/ft.icns"
if [ ! -f "$ICNS" ]; then
    echo "Error: icon not found at $ICNS"
    exit 1
fi
cp "$ICNS" "$APP_BUNDLE/Contents/Resources/ft.icns"

# --- Write Info.plist from template ---
PLIST_TEMPLATE="$PROJECT_ROOT/assets/macos/Info.plist"
if [ ! -f "$PLIST_TEMPLATE" ]; then
    echo "Error: Info.plist template not found at $PLIST_TEMPLATE"
    exit 1
fi
sed -e "s/__VERSION__/$VERSION/g" \
    -e "s/__BUILD__/$BUILD_STRING/g" \
    "$PLIST_TEMPLATE" > "$APP_BUNDLE/Contents/Info.plist"

# --- Write PkgInfo ---
echo -n "APPL????" > "$APP_BUNDLE/Contents/PkgInfo"

# Ship the exact offline verifier as a regular resource.  The detached
# manifest is emitted after codesigning so it hashes the final packaged bytes;
# later runtime-preflight work can find the verifier without executing a GUI.
cp "$ATOMIC_MANIFEST_TOOL" "$APP_BUNDLE/Contents/Resources/verify-components.sh"
chmod 0755 "$APP_BUNDLE/Contents/Resources/verify-components.sh"

# Ship the non-activating native readiness harness beside its verifier. The
# installer executes this exact manifest-bound copy against a private snapshot
# before switching the live app namespace.
NATIVE_READINESS_HARNESS="$PROJECT_ROOT/scripts/e2e_native_events.sh"
if [ ! -f "$NATIVE_READINESS_HARNESS" ] || [ -L "$NATIVE_READINESS_HARNESS" ]; then
    echo "Error: native readiness harness is unavailable or unsafe at $NATIVE_READINESS_HARNESS"
    exit 1
fi
cp "$NATIVE_READINESS_HARNESS" "$APP_BUNDLE/Contents/Resources/e2e-native-events.sh"
chmod 0755 "$APP_BUNDLE/Contents/Resources/e2e-native-events.sh"

# --- Codesign (ad-hoc) ---
if ! command -v codesign &>/dev/null; then
    echo "Error: codesign is required to produce a macOS application bundle"
    exit 1
fi
echo "Ad-hoc codesigning..."
codesign --force --deep -s - "$APP_BUNDLE"
if ! codesign --verify --deep --strict "$APP_BUNDLE"; then
    echo "Error: final application bundle failed strict deep codesign verification"
    exit 1
fi

extract_numeric_rust_const() {
    local source_file="$1"
    local const_name="$2"
    local value
    value=$(sed -n "s/^pub const ${const_name}: [^=]*= \([0-9][0-9]*\);/\1/p" "$source_file" | head -n 1)
    if [[ ! "$value" =~ ^[0-9]+$ ]]; then
        echo "Error: could not extract $const_name from $source_file" >&2
        return 1
    fi
    printf '%s\n' "$value"
}

CODEC_SOURCE="frankenterm/codec/src/lib.rs"
WIRE_SOURCE="crates/frankenterm-core/src/wire_protocol.rs"
STORAGE_SCHEMA_SOURCE="crates/frankenterm-core/src/storage/schema_ddl.rs"
CODEC_VERSION=$(extract_numeric_rust_const "$PROJECT_ROOT/$CODEC_SOURCE" CODEC_VERSION)
CODEC_MIN_SUPPORTED=$(extract_numeric_rust_const "$PROJECT_ROOT/$CODEC_SOURCE" CODEC_VERSION_MIN_SUPPORTED)
RENDER_PROTOCOL_VERSION=$(extract_numeric_rust_const "$PROJECT_ROOT/$CODEC_SOURCE" RENDER_APPLICATION_PROTOCOL_VERSION)
CORE_WIRE_PROTOCOL_VERSION=$(extract_numeric_rust_const "$PROJECT_ROOT/$WIRE_SOURCE" PROTOCOL_VERSION)
STORAGE_SCHEMA_VERSION=$(extract_numeric_rust_const "$PROJECT_ROOT/$STORAGE_SCHEMA_SOURCE" SCHEMA_VERSION)

# Generate only after all package mutation (including ad-hoc codesigning).  The
# detached manifest can therefore verify final executable bytes and the exact
# CodeResources inventory without becoming a self-referential signed resource.
bash "$ATOMIC_MANIFEST_TOOL" generate \
    --root "$APP_BUNDLE" \
    --source-root "$PROJECT_ROOT" \
    --output "$ATOMIC_MANIFEST" \
    --build-id "$FT_ATOMIC_BUILD_IDENTITY" \
    --source-revision "$SOURCE_REVISION" \
    --version "$VERSION" \
    --target "$TARGET_TRIPLE" \
    --profile "$BUILD_PROFILE" \
    --feature-contract "$FEATURE_CONTRACT" \
    --entry executable:gui:Contents/MacOS/frankenterm-gui:frankenterm-gui \
    --entry executable:mux-server:Contents/MacOS/frankenterm-mux-server:frankenterm-mux-server \
    --entry executable:pty-guardian:Contents/MacOS/frankenterm-pty-guardian:frankenterm-pty-guardian \
    --entry executable:cli:Contents/MacOS/ft:ft \
    --entry config:default-toml:Contents/Resources/frankenterm.toml \
    --entry config:default-lua:Contents/Resources/frankenterm.lua \
    --entry asset:application-icon:Contents/Resources/ft.icns \
    --entry verifier:offline-verifier:Contents/Resources/verify-components.sh \
    --entry verifier:native-readiness-harness:Contents/Resources/e2e-native-events.sh \
    --entry metadata:browser-runtime-source-manifest:Contents/Resources/browser-runtime.source-manifest.json \
    --entry metadata:info-plist:Contents/Info.plist \
    --entry metadata:package-info:Contents/PkgInfo \
    --tree font:bundled-fonts:Contents/Resources/fonts \
    --tree asset:browser-runtime:Contents/Resources/browser-runtime \
    --optional-tree asset:native-libraries:Contents/Frameworks \
    --tree metadata:native-dependencies:Contents/Resources/native-dependencies \
    --optional-tree signature:codesign:Contents/_CodeSignature \
    --source-match Contents/Resources/frankenterm.toml=crates/frankenterm-gui/frankenterm.toml \
    --source-match Contents/Resources/frankenterm.lua=crates/frankenterm-gui/frankenterm.lua \
    --source-match Contents/Resources/ft.icns=assets/macos/ft.icns \
    --source-match Contents/Resources/verify-components.sh=scripts/atomic-component-manifest.sh \
    --source-match Contents/Resources/e2e-native-events.sh=scripts/e2e_native_events.sh \
    --input workspace.manifest=Cargo.toml \
    --input protocol.codec="$CODEC_SOURCE" \
    --input protocol.core-wire="$WIRE_SOURCE" \
    --input schema.storage="$STORAGE_SCHEMA_SOURCE" \
    --input schema.atomic=docs/json-schema/ft-atomic-component-manifest.json \
    --input schema.browser-runtime-lock=docs/json-schema/ft-browser-runtime-lock-v1.json \
    --input browser.runtime-lock=docs/release/browser-runtime-lock.v1.json \
    --input schema.attestations=docs/attestations/schema.json \
    --input attestations.manifest=docs/attestations/manifest.json \
    --input default.toml=crates/frankenterm-gui/frankenterm.toml \
    --input default.lua=crates/frankenterm-gui/frankenterm.lua \
    --input font.payload=crates/frankenterm/assets/Pragmasevka_NF.zip.zst \
    --input application.icon=assets/macos/ft.icns \
    --input application.plist-template=assets/macos/Info.plist \
    --contract codec.version="$CODEC_VERSION" \
    --contract codec.min-supported="$CODEC_MIN_SUPPORTED" \
    --contract render-application.version="$RENDER_PROTOCOL_VERSION" \
    --contract core-wire.version="$CORE_WIRE_PROTOCOL_VERSION" \
    --contract storage.schema="$STORAGE_SCHEMA_VERSION" \
    --contract application.bundle-id="$BUNDLE_ID" \
    --contract browser.runtime.schema=playwright-chromium.v1 \
    --contract browser.runtime.target="$TARGET_TRIPLE" \
    --contract browser.runtime.root="$BROWSER_RUNTIME_BUNDLE_ROOT" \
    --contract browser.node.path="$BROWSER_NODE_BUNDLE_PATH" \
    --contract browser.node.version="$BROWSER_NODE_VERSION" \
    --contract browser.playwright.module-path="$BROWSER_PLAYWRIGHT_MODULE_BUNDLE_PATH" \
    --contract browser.playwright.browsers-path="$BROWSER_PLAYWRIGHT_BROWSERS_BUNDLE_PATH" \
    --contract browser.playwright.version="$BROWSER_PLAYWRIGHT_VERSION" \
    --contract browser.chromium.executable-path="$BROWSER_CHROMIUM_EXECUTABLE_BUNDLE_PATH" \
    --contract browser.chromium.revision="$BROWSER_CHROMIUM_REVISION" \
    --contract browser.protocol.version="$BROWSER_PROTOCOL_VERSION" \
    --contract browser.license.node-path="$BROWSER_NODE_LICENSE_BUNDLE_PATH" \
    --contract browser.license.playwright-path="$BROWSER_PLAYWRIGHT_LICENSE_BUNDLE_PATH" \
    --contract browser.license.chromium-path="$BROWSER_CHROMIUM_LICENSE_BUNDLE_PATH" \
    --contract browser.symlink-manifest.path="$BROWSER_SYMLINK_MANIFEST_BUNDLE_PATH" \
    --contract browser.disk-budget.bytes="$BROWSER_DISK_BUDGET_BYTES" \
    --contract browser.component.source-manifest-id="$BROWSER_SOURCE_MANIFEST_ID" \
    --contract browser.component.source-manifest-path="$BROWSER_SOURCE_MANIFEST_BUNDLE_PATH" \
    --contract panic.gui=unwind \
    --contract panic.mux-server=unwind \
    --contract panic.pty-guardian=unwind \
    --contract panic.bundled-cli=unwind

bash "$ATOMIC_MANIFEST_TOOL" verify \
    --root "$APP_BUNDLE" \
    --manifest "$ATOMIC_MANIFEST"

echo ""
echo "Done! $APP_BUNDLE"
echo "Atomic manifest: $ATOMIC_MANIFEST"
echo ""
echo "  Contents/MacOS/:"
find "$APP_BUNDLE/Contents/MacOS" -mindepth 1 -maxdepth 1 -print
echo ""
echo "  Resources:"
find "$APP_BUNDLE/Contents/Resources" -mindepth 1 -maxdepth 1 -print
echo ""
echo "To launch:  open $APP_BUNDLE"
echo "To use ft:  $APP_BUNDLE/Contents/MacOS/ft --version"
