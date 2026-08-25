#!/usr/bin/env bash
set -euo pipefail

# create-macos-bundle.sh — Build FrankenTerm.app bundle from source
#
# Builds frankenterm-gui, frankenterm-mux-server, and ft binaries, then
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
FEATURE_CONTRACT="application-family-gui-ft-mux-server-default-features-v1"
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

VERSION=$(grep -m1 '^version' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
SOURCE_REVISION=$(git -C "$PROJECT_ROOT" rev-parse HEAD)
if [[ ! "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Error: cannot resolve a full source revision for atomic packaging"
    exit 1
fi
if ! git -C "$PROJECT_ROOT" diff --quiet -- || ! git -C "$PROJECT_ROOT" diff --cached --quiet --; then
    echo "Error: tracked source changes are present; refusing to mint a commit-bound package identity"
    echo "Commit the intended source snapshot, then rebuild all components together."
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
    echo "Rebuild GUI, ft, and mux-server together from this exact source snapshot."
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
        RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 \
        run_rch --no-self-healing exec -- env \
            CARGO_TARGET_DIR="$CARGO_TARGET_DIR_REL" \
            FT_ATOMIC_BUILD_IDENTITY="$FT_ATOMIC_BUILD_IDENTITY" \
            FT_ATOMIC_BUILD_PROFILE="$BUILD_PROFILE" \
            cargo build --locked --profile "$BUILD_PROFILE" --target "$TARGET_TRIPLE" \
            --bin frankenterm-gui \
            --bin frankenterm-mux-server \
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
    # missing metadata, while success proves all three processes linked.
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
    echo "Building frankenterm-gui, frankenterm-mux-server, and bundled ft via rch ($BUILD_PROFILE, panic=unwind)..."
    if ! build_remote_bundle_with_diagnostics; then
        exit 1
    fi
fi

# --- Locate binaries ---
BINARY_DIR="$CARGO_TARGET_DIR/$TARGET_TRIPLE/$BUILD_PROFILE"
GUI_BINARY="$BINARY_DIR/frankenterm-gui"
MUX_SERVER_BINARY="$BINARY_DIR/frankenterm-mux-server"
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

echo "Installing ft CLI..."
cp "$FT_BINARY" "$APP_BUNDLE/Contents/MacOS/ft"

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
    --entry executable:cli:Contents/MacOS/ft:ft \
    --entry config:default-toml:Contents/Resources/frankenterm.toml \
    --entry config:default-lua:Contents/Resources/frankenterm.lua \
    --entry asset:application-icon:Contents/Resources/ft.icns \
    --entry verifier:offline-verifier:Contents/Resources/verify-components.sh \
    --entry metadata:browser-runtime-source-manifest:Contents/Resources/browser-runtime.source-manifest.json \
    --entry metadata:info-plist:Contents/Info.plist \
    --entry metadata:package-info:Contents/PkgInfo \
    --tree font:bundled-fonts:Contents/Resources/fonts \
    --tree asset:browser-runtime:Contents/Resources/browser-runtime \
    --optional-tree signature:codesign:Contents/_CodeSignature \
    --source-match Contents/Resources/frankenterm.toml=crates/frankenterm-gui/frankenterm.toml \
    --source-match Contents/Resources/frankenterm.lua=crates/frankenterm-gui/frankenterm.lua \
    --source-match Contents/Resources/ft.icns=assets/macos/ft.icns \
    --source-match Contents/Resources/verify-components.sh=scripts/atomic-component-manifest.sh \
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
