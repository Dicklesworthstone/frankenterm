#!/usr/bin/env bash
# tests/attestation/smoke-test.sh — round-trip test for attestation-build.sh / attestation-verify.sh.
#
# Builds a synthetic bundle from a temp manifest pointing at real files,
# verifies it, tampers with one artifact, and re-verifies (expects failure).
#
# Defined by ft-syqcz.1 (BR-RC-FOUNDATION.G3.1). Run with: bash tests/attestation/smoke-test.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

RUN_ID="${FT_ATTESTATION_SMOKE_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
WORKDIR="${FT_ATTESTATION_SMOKE_DIR:-$REPO_ROOT/target/test-artifacts/attestation-smoke/${RUN_ID}}"
OUT_DIR="$WORKDIR/attestations"
mkdir -p "$OUT_DIR"

# Stage two real files as fake "artifacts" — the schema and the manifest themselves.
# These are stable, never empty, and live in-repo.
ART_DIR="$REPO_ROOT/docs/attestations"

# Build a temp manifest that overrides only one slot.
TMP_MANIFEST="$WORKDIR/manifest.json"
cp "$ART_DIR/manifest.json" "$TMP_MANIFEST"
jq '(.slots[] | select(.category == "doctrine/agents-md-counts")).path = "docs/attestations/schema.json"' \
   "$TMP_MANIFEST" > "$TMP_MANIFEST.new" && mv "$TMP_MANIFEST.new" "$TMP_MANIFEST"

echo "=== positive build ==="
FT_ATTESTATION_MANIFEST="$TMP_MANIFEST" \
FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
  bash scripts/attestation-build.sh --version 0.0.0-smoke --channel dev --sign unsigned --allow-partial >"$WORKDIR/build.out" 2>&1
cat "$WORKDIR/build.out"
BUNDLE="$OUT_DIR/0.0.0-smoke.json"

echo
echo "=== positive verify ==="
bash scripts/attestation-verify.sh "$BUNDLE" >"$WORKDIR/verify.out"
cat "$WORKDIR/verify.out"

# Sanity: the bundle must include exactly one doctrine/agents-md-counts artifact.
count="$(jq '[.artifacts[] | select(.category == "doctrine/agents-md-counts")] | length' \
        "$BUNDLE")"
if [[ "$count" != "1" ]]; then
  echo "FAIL: expected 1 doctrine/agents-md-counts artifact, got $count"
  exit 1
fi
echo
echo "=== positive verify: artifact count OK ==="

if command -v openssl >/dev/null 2>&1 && command -v xxd >/dev/null 2>&1; then
  ED_KEY="$WORKDIR/ed25519.pem"
  ED_BUILD_VERSION="0.0.0-smoke-ed25519"
  ED_BUNDLE="$OUT_DIR/${ED_BUILD_VERSION}.json"
  ED_BAD_BUNDLE="$WORKDIR/ed25519-bad-bundle.json"
  ED_BAD_SIG="$WORKDIR/${ED_BUILD_VERSION}.bad-ed25519.sig.hex"
  ED_BAD_SIG_REL="${ED_BAD_SIG#"$REPO_ROOT"/}"

  openssl genpkey -algorithm ED25519 -out "$ED_KEY" >/dev/null 2>&1
  ED25519_PRIVATE_KEY_PATH="$ED_KEY" \
  FT_ATTESTATION_MANIFEST="$TMP_MANIFEST" \
  FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
    bash scripts/attestation-build.sh \
      --version "$ED_BUILD_VERSION" \
      --channel dev \
      --sign ed25519 \
      --allow-partial >"$WORKDIR/ed25519_build.out" 2>&1
  cat "$WORKDIR/ed25519_build.out"

  echo
  echo "=== ed25519 build + verify ==="
  bash scripts/attestation-verify.sh "$ED_BUNDLE" >"$WORKDIR/ed25519.out"
  cat "$WORKDIR/ed25519.out"

  ED_SIG_REL="$(jq -r '.signature.signature_path' "$ED_BUNDLE")"
  cp "$REPO_ROOT/$ED_SIG_REL" "$ED_BAD_SIG"
  # Flip the first byte to a different valid hex value while preserving length.
  sig_value="$(tr -d '[:space:]' < "$ED_BAD_SIG")"
  replacement="00"
  if [[ "${sig_value:0:2}" == "00" ]]; then
    replacement="ff"
  fi
  printf '%s%s\n' "$replacement" "${sig_value:2}" > "$ED_BAD_SIG"
  jq --arg sig_path "$ED_BAD_SIG_REL" '.signature.signature_path = $sig_path' \
    "$ED_BUNDLE" > "$ED_BAD_BUNDLE"

  echo
  echo "=== ed25519 bad-signature test (expect verify to FAIL) ==="
  if bash scripts/attestation-verify.sh "$ED_BAD_BUNDLE" >"$WORKDIR/ed25519_bad.out" 2>&1; then
    echo "FAIL: bad ed25519 signature verified successfully"
    cat "$WORKDIR/ed25519_bad.out"
    exit 1
  fi
  grep -q "ed25519 verify failed" "$WORKDIR/ed25519_bad.out" || {
    echo "FAIL: ed25519 verifier did not report signature failure"
    cat "$WORKDIR/ed25519_bad.out"
    exit 1
  }
  cat "$WORKDIR/ed25519_bad.out"
else
  echo
  echo "=== ed25519 verify skipped: openssl and xxd are required ==="
fi

# Tamper test: corrupt the recorded sha256 in the bundle, expect verify to fail.
TAMPERED="$WORKDIR/tampered.json"
jq '(.artifacts[0].sha256) = "0000000000000000000000000000000000000000000000000000000000000000"' \
   "$BUNDLE" > "$TAMPERED"
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
jq '.release.channel = "stable"' "$BUNDLE" > "$TAMPERED_CANON"
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
