#!/usr/bin/env bash
set -euo pipefail

# create-macos-bundle.sh — Build FrankenTerm.app bundle for macOS
#
# Creates a .app that launches wezterm-gui (the terminal emulator) with the
# FrankenTerm icon, bundled alongside the ft CLI management tool.
#
# Usage:
#   ./scripts/create-macos-bundle.sh              # build ft + bundle
#   ./scripts/create-macos-bundle.sh --skip-build  # bundle only (uses existing binaries)
#   ./scripts/create-macos-bundle.sh --output /path/to/dir  # custom output directory

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

APP_NAME="FrankenTerm"
BUNDLE_ID="com.dicklesworthstone.frankenterm"

# Source for the WezTerm GUI runtime
WEZTERM_APP="/Applications/WezTerm.app"

SKIP_BUILD=false
OUTPUT_DIR="$PROJECT_ROOT"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=true; shift ;;
        --output) OUTPUT_DIR="$2"; shift 2 ;;
        --wezterm-app) WEZTERM_APP="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--skip-build] [--output DIR] [--wezterm-app PATH]"
            echo "  --skip-build       Skip cargo build, use existing ft binary"
            echo "  --output DIR       Output directory for .app bundle (default: project root)"
            echo "  --wezterm-app PATH Path to WezTerm.app to source GUI from (default: /Applications/WezTerm.app)"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# --- Validate WezTerm source ---
WEZTERM_GUI="$WEZTERM_APP/Contents/MacOS/wezterm-gui"
if [ ! -f "$WEZTERM_GUI" ]; then
    echo "Error: wezterm-gui not found at $WEZTERM_GUI"
    echo "Install WezTerm first, or pass --wezterm-app /path/to/WezTerm.app"
    exit 1
fi

# --- Build ft ---
if [ "$SKIP_BUILD" = false ]; then
    echo "Building ft (release)..."
    cargo build --release --bin ft --manifest-path "$PROJECT_ROOT/Cargo.toml"
fi

# Locate the ft binary
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
FT_BINARY="$CARGO_TARGET_DIR/release/ft"
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
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# --- Copy WezTerm GUI binaries ---
echo "Copying wezterm-gui from $WEZTERM_APP..."
cp "$WEZTERM_GUI" "$APP_BUNDLE/Contents/MacOS/wezterm-gui"

# Also copy wezterm CLI and mux server if available
for bin in wezterm wezterm-mux-server strip-ansi-escapes; do
    src="$WEZTERM_APP/Contents/MacOS/$bin"
    if [ -f "$src" ]; then
        cp "$src" "$APP_BUNDLE/Contents/MacOS/$bin"
    fi
done

# --- Copy ft binary ---
cp "$FT_BINARY" "$APP_BUNDLE/Contents/MacOS/ft"

# --- Copy WezTerm resources (terminfo, shell completions, etc.) ---
if [ -d "$WEZTERM_APP/Contents/Resources" ]; then
    for item in "$WEZTERM_APP/Contents/Resources/"*; do
        base=$(basename "$item")
        # Skip WezTerm's icon — we use our own
        if [[ "$base" == *.icns ]]; then
            continue
        fi
        cp -R "$item" "$APP_BUNDLE/Contents/Resources/$base"
    done
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
