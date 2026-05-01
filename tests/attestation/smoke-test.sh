#!/usr/bin/env bash
# tests/attestation/smoke-test.sh — round-trip test for attestation-build.sh / attestation-verify.sh.
#
# Builds a synthetic bundle from a temp manifest pointing at real files, verifies it,
# tampers with one artifact, re-verifies (expects failure), and tears down.
#
# Defined by ft-syqcz.1 (BR-RC-FOUNDATION.G3.1). Run with: bash tests/attestation/smoke-test.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Stage two real files as fake "artifacts" — the schema and the manifest themselves.
# These are stable, never empty, and live in-repo.
ART_DIR="$REPO_ROOT/docs/attestations"

# Build a temp manifest that overrides only one slot.
TMP_MANIFEST="$WORKDIR/manifest.json"
cp "$ART_DIR/manifest.json" "$TMP_MANIFEST"
jq '(.slots[] | select(.category == "doctrine/agents-md-counts")).path = "docs/attestations/schema.json"' \
   "$TMP_MANIFEST" > "$TMP_MANIFEST.new" && mv "$TMP_MANIFEST.new" "$TMP_MANIFEST"

# Stash the real manifest, swap in the temp one for the duration of the test.
BACKUP="$WORKDIR/manifest.bak.json"
cp "$ART_DIR/manifest.json" "$BACKUP"
cp "$TMP_MANIFEST" "$ART_DIR/manifest.json"
restore_manifest() {
  cp "$BACKUP" "$ART_DIR/manifest.json"
  rm -f "$ART_DIR/0.0.0-smoke.json" "$ART_DIR/0.0.0-smoke.canonical.json" "$ART_DIR/0.0.0-smoke.sigstore"
}
trap 'restore_manifest; rm -rf "$WORKDIR"' EXIT

echo "=== positive build ==="
bash scripts/attestation-build.sh --version 0.0.0-smoke --channel dev --sign unsigned --allow-partial >"$WORKDIR/build.out" 2>&1
cat "$WORKDIR/build.out"

echo
echo "=== positive verify ==="
bash scripts/attestation-verify.sh "docs/attestations/0.0.0-smoke.json" >"$WORKDIR/verify.out"
cat "$WORKDIR/verify.out"

# Sanity: the bundle must include exactly one doctrine/agents-md-counts artifact.
count="$(jq '[.artifacts[] | select(.category == "doctrine/agents-md-counts")] | length' \
        "$ART_DIR/0.0.0-smoke.json")"
if [[ "$count" != "1" ]]; then
  echo "FAIL: expected 1 doctrine/agents-md-counts artifact, got $count"
  exit 1
fi
echo
echo "=== positive verify: artifact count OK ==="

# Tamper test: corrupt the recorded sha256 in the bundle, expect verify to fail.
TAMPERED="$WORKDIR/tampered.json"
jq '(.artifacts[0].sha256) = "0000000000000000000000000000000000000000000000000000000000000000"' \
   "$ART_DIR/0.0.0-smoke.json" > "$TAMPERED"
echo
echo "=== tamper test (expect verify to FAIL) ==="
if bash scripts/attestation-verify.sh "$TAMPERED" >"$WORKDIR/tamper.out" 2>&1; then
  echo "FAIL: tampered bundle verified successfully — verify is broken"
  cat "$WORKDIR/tamper.out"
  exit 1
fi
cat "$WORKDIR/tamper.out"
echo
echo "=== tamper detected — verify failed loudly as expected ==="

# Canonical-payload tamper: change a top-level field, expect canonical_sha256 mismatch.
TAMPERED_CANON="$WORKDIR/tampered_canon.json"
jq '.release.channel = "stable"' "$ART_DIR/0.0.0-smoke.json" > "$TAMPERED_CANON"
echo
echo "=== canonical tamper test (expect verify to FAIL on canonical_sha256) ==="
if bash scripts/attestation-verify.sh "$TAMPERED_CANON" >"$WORKDIR/canon.out" 2>&1; then
  echo "FAIL: canonical-tampered bundle verified successfully"
  cat "$WORKDIR/canon.out"
  exit 1
fi
grep -q "canonical_sha256" "$WORKDIR/canon.out" || { echo "FAIL: canonical_sha256 check did not fire"; cat "$WORKDIR/canon.out"; exit 1; }
echo "=== canonical tamper detected ==="

echo
echo "PASS: attestation build + verify round-trip clean"
