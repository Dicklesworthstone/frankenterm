#!/bin/bash
# build-apple-machines-helper.sh — build the Mac companion's machines helper
#
# Builds crates/frankenterm-machines for Apple-silicon macOS and places it at
# apps/macos/FrankenTermExport/Artifacts/frankenterm-machines, where the
# FrankenTermExport target's "Bundle frankenterm-machines helper" phase copies
# and signs it into the app. At launch the companion runs
# `frankenterm-machines source` to load this Mac's machines.toml and the keys
# its routes use (paths only; the companion reads and seals the keys itself).
#
# A local native development build, like build-apple-terminal-xcframework.sh:
# Cargo runs locally on purpose (RCH workers are Linux), via the shim bypass.
# It is not RCH proof and not a release artifact. An existing helper is moved
# aside under Artifacts/previous/, never deleted.
#
# Usage:
#   scripts/build-apple-machines-helper.sh
#   FT_APPLE_TARGET_DIR=/path scripts/build-apple-machines-helper.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARTIFACTS="$ROOT/apps/macos/FrankenTermExport/Artifacts"
OUTPUT="$ARTIFACTS/frankenterm-machines"
TARGET_DIR="${FT_APPLE_TARGET_DIR:-$HOME/ft-build/apple-machines-helper}"
export MACOSX_DEPLOYMENT_TARGET=14.0

(cd "$ROOT" && RCH_CARGO_WRAPPER_BYPASS=1 CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --release -p frankenterm-machines --target aarch64-apple-darwin)
built="$TARGET_DIR/aarch64-apple-darwin/release/frankenterm-machines"
file "$built" | grep -q "Mach-O 64-bit executable arm64" || { echo "error: $built is not an arm64 Mach-O" >&2; exit 1; }

mkdir -p "$ARTIFACTS"
if [ -e "$OUTPUT" ]; then
    aside="$ARTIFACTS/previous/$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$aside"
    mv "$OUTPUT" "$aside/"
    echo "==> previous helper moved to $aside"
fi
cp "$built" "$OUTPUT"
echo "==> $OUTPUT"
echo "    source $(cd "$ROOT" && git rev-parse --short HEAD)$(cd "$ROOT" && git diff --quiet -- frankenterm crates Cargo.toml Cargo.lock || echo ' (dirty tree)')"
