#!/bin/bash
# build-apple-ssh-xcframework.sh — libssh2 + OpenSSL for the native Apple SSH driver
#
# The FrankenTermSSH package drives the plan's selected libssh2 backend
# (apps/apple/FrankenTermSSH). On Apple platforms it links
# apps/apple/FrankenTermSSH/Artifacts/CLibssh2.xcframework, built here for iOS
# devices (arm64), the arm64 iOS simulator and Apple-silicon macOS.
#
# Sources are the exact copies Cargo.lock already pins and checksums: libssh2
# from the libssh2-sys crate and OpenSSL from the openssl-src crate (fetched
# with `cargo fetch --locked` if absent). Each is built with its own upstream
# build system (OpenSSL Configure, libssh2 CMake), static only, with the
# OpenSSL crypto backend that the driver's Ed25519/ECDSA/curve25519 profile
# needs. Each slice's libssh2.a and libcrypto.a are merged into one archive so
# the binary target is self-contained.
#
# Local native development build (Xcode, cmake and perl required). Not a release
# artifact; DSR owns release packaging. An existing artifact is renamed aside
# under Artifacts/previous/, never deleted.
#
# Usage:
#   scripts/build-apple-ssh-xcframework.sh
#   FT_APPLE_SSH_BUILD_DIR=/path scripts/build-apple-ssh-xcframework.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
IOS_MIN="${IPHONEOS_DEPLOYMENT_TARGET:-26.0}"
MACOS_MIN="${MACOSX_DEPLOYMENT_TARGET:-14.0}"
BUILD="${FT_APPLE_SSH_BUILD_DIR:-$HOME/ft-build/frankenterm-apple-ssh-ios${IOS_MIN}-macos${MACOS_MIN}}"
ARTIFACTS="$ROOT/apps/apple/FrankenTermSSH/Artifacts"
OUTPUT="$ARTIFACTS/CLibssh2.xcframework"
JOBS="$(sysctl -n hw.ncpu)"

for tool in xcodebuild xcrun cmake perl libtool make; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

locked_version() {
    awk -v name="$1" '$0 == "name = \"" name "\"" { getline; gsub(/version = |"/, ""); print; exit }' "$ROOT/Cargo.lock"
}
crate_source() {
    local name="$1" version
    version="$(locked_version "$name")"
    [ -n "$version" ] || { echo "error: $name is not in Cargo.lock" >&2; exit 1; }
    local home="${CARGO_HOME:-$HOME/.cargo}" path
    path="$(ls -d "$home"/registry/src/*/"$name-$version" 2>/dev/null | head -1 || true)"
    if [ -z "$path" ]; then
        (cd "$ROOT" && RCH_CARGO_WRAPPER_BYPASS=1 cargo fetch --locked)
        path="$(ls -d "$home"/registry/src/*/"$name-$version" 2>/dev/null | head -1 || true)"
    fi
    [ -n "$path" ] || { echo "error: no registry source for $name $version" >&2; exit 1; }
    echo "$path"
}

LIBSSH2_SRC="$(crate_source libssh2-sys)/libssh2"
OPENSSL_SRC="$(crate_source openssl-src)/openssl"
[ -f "$LIBSSH2_SRC/CMakeLists.txt" ] || { echo "error: $LIBSSH2_SRC lacks CMakeLists.txt" >&2; exit 1; }
[ -x "$OPENSSL_SRC/Configure" ] || { echo "error: $OPENSSL_SRC lacks Configure" >&2; exit 1; }
echo "==> libssh2 $(awk -F'"' '/#define LIBSSH2_VERSION /{print $2}' "$LIBSSH2_SRC/include/libssh2.h") from $LIBSSH2_SRC"
echo "==> OpenSSL $(awk -F= '/^MAJOR|^MINOR|^PATCH/{printf "%s.", $2}' "$OPENSSL_SRC/VERSION.dat" | sed 's/\.$//') from $OPENSSL_SRC"

# slice: name, OpenSSL target, minimum-version flag, SDK, CMake system name
build_slice() {
    local slice="$1" ossl_target="$2" min_flag="$3" sdk="$4" system="$5"
    local prefix="$BUILD/install/$slice"
    local sysroot
    sysroot="$(xcrun --sdk "$sdk" --show-sdk-path)"
    echo "==> OpenSSL $ossl_target ($slice)"
    mkdir -p "$BUILD/openssl/$slice" "$prefix"
    (cd "$BUILD/openssl/$slice" &&
        perl "$OPENSSL_SRC/Configure" "$ossl_target" no-shared no-tests no-apps no-docs \
            no-ui-console no-engine no-module no-dso "$min_flag" \
            --prefix="$prefix" --openssldir="$prefix/ssl" --libdir=lib >/dev/null &&
        make -j"$JOBS" build_libs >/dev/null &&
        make install_dev >/dev/null)
    echo "==> libssh2 ($slice)"
    # Explicit OpenSSL paths: an iOS CMake build searches only inside the SDK
    # sysroot, so OPENSSL_ROOT_DIR alone is ignored there.
    local system_arg=()
    [ -n "$system" ] && system_arg=(-DCMAKE_SYSTEM_NAME="$system")
    cmake -S "$LIBSSH2_SRC" -B "$BUILD/libssh2/$slice" -DCMAKE_BUILD_TYPE=Release \
        ${system_arg[@]+"${system_arg[@]}"} -DCMAKE_OSX_SYSROOT="$sysroot" -DCMAKE_OSX_ARCHITECTURES=arm64 \
        -DCMAKE_OSX_DEPLOYMENT_TARGET="${min_flag#*=}" \
        -DCRYPTO_BACKEND=OpenSSL -DOPENSSL_USE_STATIC_LIBS=TRUE \
        -DOPENSSL_INCLUDE_DIR="$prefix/include" -DOPENSSL_CRYPTO_LIBRARY="$prefix/lib/libcrypto.a" \
        -DOPENSSL_SSL_LIBRARY="$prefix/lib/libssl.a" \
        -DBUILD_SHARED_LIBS=OFF -DBUILD_STATIC_LIBS=ON -DBUILD_EXAMPLES=OFF -DBUILD_TESTING=OFF \
        -DENABLE_ZLIB_COMPRESSION=OFF -DCMAKE_INSTALL_PREFIX="$prefix" >/dev/null
    cmake --build "$BUILD/libssh2/$slice" -j "$JOBS" >/dev/null
    cmake --install "$BUILD/libssh2/$slice" >/dev/null
    mkdir -p "$BUILD/merged/$slice"
    libtool -static -o "$BUILD/merged/$slice/libssh2-openssl.a" "$prefix/lib/libssh2.a" "$prefix/lib/libcrypto.a" 2>/dev/null
}

build_slice ios-arm64 ios64-xcrun "-mios-version-min=$IOS_MIN" iphoneos iOS
build_slice ios-arm64-simulator iossimulator-arm64-xcrun "-mios-simulator-version-min=$IOS_MIN" iphonesimulator iOS
build_slice macos-arm64 darwin64-arm64-cc "-mmacosx-version-min=$MACOS_MIN" macosx ""

headers="$(mktemp -d "${TMPDIR:-/tmp}/ft-libssh2-headers.XXXXXX")"
cp "$BUILD/install/ios-arm64/include/libssh2.h" "$BUILD/install/ios-arm64/include/libssh2_publickey.h" \
   "$BUILD/install/ios-arm64/include/libssh2_sftp.h" "$headers/"
printf 'module CLibssh2 {\n    header "libssh2.h"\n    export *\n}\n' > "$headers/module.modulemap"

staging="$(mktemp -d "${TMPDIR:-/tmp}/ft-libssh2-xcframework.XXXXXX")"
xcodebuild -create-xcframework \
    -library "$BUILD/merged/ios-arm64/libssh2-openssl.a" -headers "$headers" \
    -library "$BUILD/merged/ios-arm64-simulator/libssh2-openssl.a" -headers "$headers" \
    -library "$BUILD/merged/macos-arm64/libssh2-openssl.a" -headers "$headers" \
    -output "$staging/CLibssh2.xcframework" >/dev/null

mkdir -p "$ARTIFACTS"
if [ -e "$OUTPUT" ]; then
    aside="$ARTIFACTS/previous/$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$aside"
    mv "$OUTPUT" "$aside/"
    echo "==> previous artifact moved to $aside"
fi
mv "$staging/CLibssh2.xcframework" "$OUTPUT"
echo "==> $OUTPUT"
