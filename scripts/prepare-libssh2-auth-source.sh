#!/bin/bash
# Prepare the narrowly patched libssh2 used for signed-key MFA continuation.
# The Cargo registry remains untouched. The patch accepts only the reviewed
# source bytes, and every cached result is checked before reuse.
# Usage: bash scripts/prepare-libssh2-auth-source.sh UPSTREAM_SOURCE CACHE_DIR
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCH="$SCRIPT_DIR/patches/libssh2-auth-partial.patch"
UPSTREAM_SUMS="$SCRIPT_DIR/patches/libssh2-auth-partial.upstream.sha256"
[ "$#" -eq 2 ] || { echo "usage: $0 UPSTREAM_SOURCE CACHE_DIR" >&2; exit 1; }
for tool in shasum patch; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done
SOURCE="$(cd "$1" && pwd)"
mkdir -p "$2"
CACHE="$(cd "$2" && pwd)"

# A changed Cargo vendor snapshot must receive a deliberate patch review.
(cd "$SOURCE" && shasum -a 256 -c "$UPSTREAM_SUMS" >&2)
SOURCE_DIGEST="$(cat "$PATCH" "$UPSTREAM_SUMS" | shasum -a 256 | awk '{print $1}')"
PREPARED="$CACHE/libssh2-auth-$SOURCE_DIGEST"
if [ -e "$PREPARED" ]; then
    [ -f "$PREPARED/.frankenterm-auth-source.sha256" ] || {
        echo "error: incomplete patched source at $PREPARED; preserve it and choose a fresh cache directory" >&2
        exit 1
    }
    cmp "$PATCH" "$PREPARED/.frankenterm-auth.patch" >/dev/null
    (cd "$PREPARED" && shasum -a 256 -c .frankenterm-auth-source.sha256 >&2)
else
    # Retain incomplete staging on failure for diagnosis; no source is removed.
    STAGING="$(mktemp -d "$CACHE/libssh2-auth-staging.XXXXXX")"
    cp -R "$SOURCE/." "$STAGING/"
    (cd "$STAGING" && patch -N -f -F 0 -p1 -i "$PATCH" >&2)
    cp "$PATCH" "$STAGING/.frankenterm-auth.patch"
    (cd "$STAGING" && shasum -a 256 include/libssh2.h src/libssh2_priv.h src/userauth.c \
        > .frankenterm-auth-source.sha256)
    mv "$STAGING" "$PREPARED"
fi
printf '%s\n' "$PREPARED"
