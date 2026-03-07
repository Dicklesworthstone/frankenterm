#!/usr/bin/env bash
set -euo pipefail

# create-macos-bundle.sh — Build FrankenTerm.app bundle from source
#
# Builds frankenterm-gui and ft binaries, then packages them into a macOS
# .app bundle with the FrankenTerm icon and Info.plist.
#
# No dependency on a pre-installed WezTerm.app.
#
# Usage:
#   ./scripts/create-macos-bundle.sh               # build everything + bundle
#   ./scripts/create-macos-bundle.sh --skip-build # bundle only (uses existing binaries)
#   ./scripts/create-macos-bundle.sh --output /path/to/dir  # custom output directory
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

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=true; shift ;;
        --output) OUTPUT_DIR="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--skip-build] [--output DIR]"
            echo "  --skip-build  Skip cargo build, use existing binaries"
            echo "  --output DIR  Output directory for .app bundle (default: project root)"
            echo "                Existing FrankenTerm.app bundles are not overwritten."
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"

require_rch() {
    command -v "$RCH_BIN" >/dev/null 2>&1
}

probe_rch_workers() {
    local probe_json
    probe_json="$("$RCH_BIN" workers probe --json --all)"
    python3 - "$probe_json" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
for worker in payload.get("data", []):
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

# --- Build from source ---
if [ "$SKIP_BUILD" = false ]; then
    if ! require_rch; then
        echo "Error: rch not found at '$RCH_BIN'"
        exit 1
    fi
    if ! probe_rch_workers; then
        echo "Error: no reachable RCH workers detected; refusing local cargo fallback"
        exit 1
    fi
    echo "Building frankenterm-gui and ft via rch (release)..."
    "$RCH_BIN" exec -- env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release \
        --bin frankenterm-gui \
        --bin ft \
        --manifest-path "$PROJECT_ROOT/Cargo.toml"
fi

# --- Locate binaries ---
GUI_BINARY="$CARGO_TARGET_DIR/release/frankenterm-gui"
FT_BINARY="$CARGO_TARGET_DIR/release/ft"

if [ ! -f "$GUI_BINARY" ]; then
    echo "Error: frankenterm-gui binary not found at $GUI_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi
if [ ! -f "$FT_BINARY" ]; then
    echo "Error: ft binary not found at $FT_BINARY"
    echo "Run without --skip-build, or set CARGO_TARGET_DIR."
    exit 1
fi

# --- Version ---
VERSION=$(grep -m1 '^version' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
BUILD_STRING=$(date -u +%Y%m%d.%H%M%S)

echo "Packaging $APP_NAME.app v$VERSION (build $BUILD_STRING)..."

# --- Bundle structure ---
APP_BUNDLE="$OUTPUT_DIR/$APP_NAME.app"
if [ -e "$APP_BUNDLE" ]; then
    echo "Error: app bundle already exists at $APP_BUNDLE"
    echo "Choose a fresh --output directory or remove the existing bundle manually."
    exit 1
fi
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# --- Copy binaries built from source ---
echo "Installing frankenterm-gui..."
cp "$GUI_BINARY" "$APP_BUNDLE/Contents/MacOS/frankenterm-gui"

echo "Installing ft CLI..."
cp "$FT_BINARY" "$APP_BUNDLE/Contents/MacOS/ft"

# --- Copy default config ---
DEFAULT_CONFIG="$PROJECT_ROOT/crates/frankenterm-gui/frankenterm.toml"
if [ -f "$DEFAULT_CONFIG" ]; then
    cp "$DEFAULT_CONFIG" "$APP_BUNDLE/Contents/Resources/frankenterm.toml"
fi

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

# --- Codesign (ad-hoc) ---
if command -v codesign &>/dev/null; then
    echo "Ad-hoc codesigning..."
    codesign --force --deep -s - "$APP_BUNDLE"
fi

echo ""
echo "Done! $APP_BUNDLE"
echo ""
echo "  Contents/MacOS/:"
ls -lh "$APP_BUNDLE/Contents/MacOS/" | tail -n +2
echo ""
echo "  Resources:"
ls "$APP_BUNDLE/Contents/Resources/"
echo ""
echo "To launch:  open $APP_BUNDLE"
echo "To use ft:  $APP_BUNDLE/Contents/MacOS/ft --version"
