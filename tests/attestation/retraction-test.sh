#!/usr/bin/env bash
# tests/attestation/retraction-test.sh — signed retraction roundtrip.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

command -v jq >/dev/null 2>&1 || { echo "jq required" >&2; exit 2; }

if ! command -v openssl >/dev/null 2>&1 || ! command -v xxd >/dev/null 2>&1; then
  echo "SKIP: openssl and xxd are required for ed25519 retraction coverage"
  exit 0
fi

sha256_file() {
  local f="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" | awk '{print $1}'
  else
    shasum -a 256 "$f" | awk '{print $1}'
  fi
}
sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

RUN_ID="${FT_ATTESTATION_RETRACTION_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
RUN_ROOT="${FT_ATTESTATION_RETRACTION_LOG_DIR:-$REPO_ROOT/target/test-logs/attestation-retraction/$RUN_ID}"
OUT_DIR="$RUN_ROOT/attestations"
RETRACTIONS_ROOT="$RUN_ROOT/retractions"
MANIFEST="$RUN_ROOT/manifest.json"
RATIONALE="$RUN_ROOT/rationale.txt"
KEY="$RUN_ROOT/ed25519.pem"
mkdir -p "$OUT_DIR" "$RETRACTIONS_ROOT"

cat > "$MANIFEST" <<'JSON'
{
  "$schema": "./schema.json#/$defs/manifestPlaceholder",
  "required_categories": ["doctrine/agents-md-counts"],
  "slots": [
    {
      "category": "doctrine/agents-md-counts",
      "path": "docs/attestations/schema.json",
      "media_type": "application/json",
      "produced_by_bead": "ft-tf6g3.2",
      "proof_categories": [5],
      "description": "fixture slot for signed retraction roundtrip"
    }
  ]
}
JSON
cat > "$RATIONALE" <<'TXT'
Fixture retraction: the original doctrine count claim was superseded by a corrected release note.
TXT
openssl genpkey -algorithm ED25519 -out "$KEY" >/dev/null 2>&1

echo "=== build baseline bundle ==="
FT_ATTESTATION_MANIFEST="$MANIFEST" \
FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
FT_ATTESTATION_RETRACTIONS_ROOT="$RETRACTIONS_ROOT" \
  bash scripts/attestation-build.sh \
    --version 0.0.0-retraction \
    --channel dev \
    --sign unsigned \
    --allow-partial > "$RUN_ROOT/build-baseline.out" 2>&1
cat "$RUN_ROOT/build-baseline.out"
BUNDLE="$OUT_DIR/0.0.0-retraction.json"

echo
echo "=== verify baseline bundle ==="
FT_ATTESTATION_RETRACTIONS_ROOT="$RETRACTIONS_ROOT" \
  bash scripts/attestation-verify.sh "$BUNDLE" --json > "$RUN_ROOT/verify-baseline.json"
jq -e '.ok == true and .verdict == "pass"' "$RUN_ROOT/verify-baseline.json" >/dev/null

echo
echo "=== create signed retraction ==="
ED25519_PRIVATE_KEY_PATH="$KEY" \
FT_ATTESTATION_RETRACTIONS_ROOT="$RETRACTIONS_ROOT" \
  bash scripts/retract-bundle-slot.sh \
    --bundle "$BUNDLE" \
    --slot doctrine/agents-md-counts \
    --rationale-file "$RATIONALE" \
    --retracted-by-release 0.0.1-corrigendum \
    --sign ed25519 > "$RUN_ROOT/retract.out" 2>&1
cat "$RUN_ROOT/retract.out"

echo
echo "=== verify returns retracted verdict ==="
set +e
FT_ATTESTATION_RETRACTIONS_ROOT="$RETRACTIONS_ROOT" \
  bash scripts/attestation-verify.sh "$BUNDLE" --json > "$RUN_ROOT/verify-retracted.json" 2>&1
rc=$?
set -e
if [[ "$rc" -ne 3 ]]; then
  echo "FAIL: expected retracted exit code 3, got $rc"
  cat "$RUN_ROOT/verify-retracted.json"
  exit 1
fi
jq -e '
  .ok == false
  and .verdict == "retracted"
  and (.retractions | length) == 1
  and .retractions[0].affected_slot == "doctrine/agents-md-counts"
' "$RUN_ROOT/verify-retracted.json" >/dev/null

echo
echo "=== next bundle indexes active retraction ==="
FT_ATTESTATION_MANIFEST="$MANIFEST" \
FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
FT_ATTESTATION_RETRACTIONS_ROOT="$RETRACTIONS_ROOT" \
  bash scripts/attestation-build.sh \
    --version 0.0.1-retraction \
    --channel dev \
    --sign unsigned \
    --allow-partial > "$RUN_ROOT/build-with-retraction.out" 2>&1
jq -e '
  (.retractions | length) == 1
  and .retractions[0].affected_slot == "doctrine/agents-md-counts"
' "$OUT_DIR/0.0.1-retraction.json" >/dev/null

echo
echo "=== unsigned retraction is rejected ==="
FT_ATTESTATION_MANIFEST="$MANIFEST" \
FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
FT_ATTESTATION_RETRACTIONS_ROOT="$RUN_ROOT/unsigned-retractions" \
  bash scripts/attestation-build.sh \
    --version 0.0.2-retraction \
    --channel dev \
    --sign unsigned \
    --allow-partial > "$RUN_ROOT/build-unsigned-negative.out" 2>&1
UNSIGNED_BUNDLE="$OUT_DIR/0.0.2-retraction.json"
UNSIGNED_SHA="$(sha256_file "$UNSIGNED_BUNDLE")"
UNSIGNED_DIR="$RUN_ROOT/unsigned-retractions/$UNSIGNED_SHA"
mkdir -p "$UNSIGNED_DIR"
UNSIGNED_RETRACTION="$UNSIGNED_DIR/doctrine__agents-md-counts.json"
ORIGINAL_CLAIM="$(jq -c '.artifacts[0]' "$UNSIGNED_BUNDLE")"
UNSIGNED_NO_SIG="$(jq -n \
  --arg schema_version "1.0.0" \
  --arg retracted_at "2026-05-13T00:00:00Z" \
  --arg retracted_by_release "0.0.3-corrigendum" \
  --arg retraction_rationale "unsigned fixture must be rejected" \
  --arg affected_slot "doctrine/agents-md-counts" \
  --arg original_bundle_sha256 "$UNSIGNED_SHA" \
  --argjson original_claim_value "$ORIGINAL_CLAIM" \
  '{
    schema_version: $schema_version,
    retracted_at: $retracted_at,
    retracted_by_release: $retracted_by_release,
    retraction_rationale: $retraction_rationale,
    affected_slot: $affected_slot,
    original_bundle_sha256: $original_bundle_sha256,
    original_claim_value: $original_claim_value,
    corrected_claim_value: null
  }')"
UNSIGNED_CANONICAL_PAYLOAD="$(jq -S -c '.' <<<"$UNSIGNED_NO_SIG")"
UNSIGNED_CANONICAL_SHA="$(printf '%s' "$UNSIGNED_CANONICAL_PAYLOAD" | sha256_stdin)"
jq --arg canonical_sha256 "$UNSIGNED_CANONICAL_SHA" \
  '. + {retraction_signature: {method: "unsigned", canonical_sha256: $canonical_sha256, reason: "negative fixture"}}' \
  <<<"$UNSIGNED_NO_SIG" > "$UNSIGNED_RETRACTION"

set +e
FT_ATTESTATION_RETRACTIONS_ROOT="$RUN_ROOT/unsigned-retractions" \
  bash scripts/attestation-verify.sh "$UNSIGNED_BUNDLE" --json > "$RUN_ROOT/verify-unsigned-retraction.json" 2>&1
rc=$?
set -e
if [[ "$rc" -eq 0 || "$rc" -eq 3 ]]; then
  echo "FAIL: unsigned retraction was accepted with rc=$rc"
  cat "$RUN_ROOT/verify-unsigned-retraction.json"
  exit 1
fi
jq -e '.verdict == "fail" and (.errors | join(" ") | test("unsigned retractions are rejected"))' \
  "$RUN_ROOT/verify-unsigned-retraction.json" >/dev/null

echo
echo "PASS: attestation retraction roundtrip"
