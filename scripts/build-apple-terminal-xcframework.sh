#!/bin/bash
# build-apple-terminal-xcframework.sh — package the canonical terminal C ABI
#
# Builds frankenterm-term with only the `native-ffi` feature as a static library
# for iOS devices (aarch64-apple-ios), the arm64 iOS simulator
# (aarch64-apple-ios-sim) and Apple-silicon macOS (aarch64-apple-darwin), then
# packages the three slices with the checked-in header
# (frankenterm/term/include/frankenterm_terminal.h) as
# apps/apple/FrankenTermTerminal/Artifacts/CFrankenTermTerminal.xcframework,
# the binary target that the FrankenTermTerminal Swift package links on Apple
# hosts. The macOS slice lets `swift test` exercise the Swift binding against
# the real Rust engine on a Mac.
#
# This is a local native development build: it needs Xcode (the iOS SDKs) and
# the pinned nightly's iOS targets (`rustup target add aarch64-apple-ios
# aarch64-apple-ios-sim`). It is not RCH proof of the Rust code and not a release
# artifact; DSR owns release packaging. Cargo runs locally on purpose (RCH
# workers are Linux and cannot link Apple targets), via the shim bypass.
#
# The profile keeps panic=unwind (release-interactive), which native-ffi requires.
# An existing artifact is renamed aside under Artifacts/previous/, never deleted.
#
# Usage:
#   scripts/build-apple-terminal-xcframework.sh
#   FT_APPLE_TARGET_DIR=/path scripts/build-apple-terminal-xcframework.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROFILE="${FT_APPLE_PROFILE:-release-interactive}"
# Match the Swift packages' platform floors, otherwise the objects carry the
# host SDK's minimum version and every link warns (or fails on older devices).
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-26.0}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-14.0}"
# Keyed by deployment targets: Cargo does not rebuild when only they change.
TARGET_DIR="${FT_APPLE_TARGET_DIR:-$HOME/ft-build/frankenterm-apple-terminal-ios${IPHONEOS_DEPLOYMENT_TARGET}-macos${MACOSX_DEPLOYMENT_TARGET}}"
ARTIFACTS="$ROOT/apps/apple/FrankenTermTerminal/Artifacts"
OUTPUT="$ARTIFACTS/CFrankenTermTerminal.xcframework"
HEADER="$ROOT/frankenterm/term/include/frankenterm_terminal.h"
TARGETS="aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin"

command -v xcodebuild >/dev/null || { echo "error: xcodebuild not found (Xcode is required)" >&2; exit 1; }
[ -f "$HEADER" ] || { echo "error: missing $HEADER" >&2; exit 1; }
installed="$(cd "$ROOT" && rustup target list --installed)"
for target in $TARGETS; do
    case "$installed" in
        *"$target"*) ;;
        *) echo "error: Rust target $target is not installed for this toolchain; run: rustup target add $target" >&2; exit 1 ;;
    esac
done

libraries=""
for target in $TARGETS; do
    echo "==> frankenterm-term native-ffi staticlib for $target ($PROFILE)"
    (cd "$ROOT" && RCH_CARGO_WRAPPER_BYPASS=1 CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo rustc --locked -p frankenterm-term --lib --features native-ffi \
        --crate-type staticlib --target "$target" --profile "$PROFILE")
    library="$TARGET_DIR/$target/$PROFILE/libfrankenterm_term.a"
    [ -f "$library" ] || { echo "error: cargo did not produce $library" >&2; exit 1; }
    libraries="$libraries $library"
done

# Every slice must export exactly the functions the header declares.
expected="$(grep -oE 'ft_terminal_v1_[a-z_]+\(' "$HEADER" | tr -d '(' | sort -u)"
for library in $libraries; do
    # nm exits nonzero for archive members without symbols; the comparison below
    # is the check, so its status is not.
    exported="$({ nm -gU "$library" 2>/dev/null || true; } | awk '$2 == "T" && $3 ~ /^_ft_terminal_v1_/ { sub(/^_/, "", $3); print $3 }' | sort -u)"
    if [ "$exported" != "$expected" ]; then
        echo "error: $library does not export exactly the header's ft_terminal_v1_* ABI" >&2
        diff <(echo "$expected") <(echo "$exported") >&2 || true
        exit 1
    fi
done

headers="$(mktemp -d "${TMPDIR:-/tmp}/ft-terminal-headers.XXXXXX")"
cp "$HEADER" "$headers/"
printf 'module CFrankenTermTerminal {\n    header "frankenterm_terminal.h"\n    export *\n}\n' > "$headers/module.modulemap"

staging="$(mktemp -d "${TMPDIR:-/tmp}/ft-terminal-xcframework.XXXXXX")"
arguments=()
for library in $libraries; do
    arguments+=(-library "$library" -headers "$headers")
done
xcodebuild -create-xcframework "${arguments[@]}" -output "$staging/CFrankenTermTerminal.xcframework"

mkdir -p "$ARTIFACTS"
if [ -e "$OUTPUT" ]; then
    aside="$ARTIFACTS/previous/$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$aside"
    mv "$OUTPUT" "$aside/"
    echo "==> previous artifact moved to $aside"
fi
mv "$staging/CFrankenTermTerminal.xcframework" "$OUTPUT"
echo "==> $OUTPUT"
echo "    source $(cd "$ROOT" && git rev-parse --short HEAD)$(cd "$ROOT" && git diff --quiet -- frankenterm crates Cargo.toml Cargo.lock || echo ' (dirty tree)')"
