#!/usr/bin/env bash
# G51 / ft-tf6g3.39: regenerate the verify-the-verifier self-test fixtures.
#
# The positive-control fixture (all_sections_present_valid.json) embeds
# sha256 hashes of artifacts in docs/perf/, docs/attestations/, etc.
# When sibling agents update those upstream files, the fixture's
# recorded hashes go stale and the verify-the-verifier positive control
# starts (correctly) reporting ok=false.
#
# Run this script to regenerate the fixtures against the current HEAD
# state. Commit the updated fixtures to keep
# tests/scripts/test_attestation_verify_self_test.sh passing.
#
# Usage:
#   scripts/regenerate-attestation-verify-fixtures.sh
#
# Workflow:
#   1. Build a fresh positive-control fixture from current HEAD via
#      scripts/attestation-build.sh --version all_sections_present_valid
#      --channel dev --sign unsigned --allow-partial, writing directly into
#      the fixture directory.
#   2. Apply the documented jq mutations to derive the tampered variants.
#   3. Refresh the unsigned canonical hash for every tamper whose expected
#      failure is not itself a signature/canonical-hash failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIX="${REPO_ROOT}/tests/fixtures/attestation-verify-self-test/fixtures"
POSITIVE="${FIX}/all_sections_present_valid.json"

command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }
[[ -x "${REPO_ROOT}/scripts/attestation-build.sh" ]] || {
  echo "attestation-build.sh not executable" >&2; exit 2;
}
[[ -d "$FIX" ]] || { echo "$FIX missing" >&2; exit 2; }

cd "$REPO_ROOT"

sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

refresh_unsigned_canonical_sha() {
  local fixture="$1"
  local canonical_payload canonical_sha tmp

  canonical_payload="$(jq -S -c 'del(.signature)' "${fixture}")"
  canonical_sha="$(printf '%s' "${canonical_payload}" | sha256_stdin)"
  tmp="${fixture}.next"
  jq --arg canonical_sha "${canonical_sha}" \
    '.signature.canonical_sha256 = $canonical_sha' \
    "${fixture}" > "${tmp}"
  mv "${tmp}" "${fixture}"
}

echo "1/3: Building fresh positive-control fixture from current HEAD..." >&2
FT_ATTESTATION_OUT_DIR="${FIX}" \
  ./scripts/attestation-build.sh \
    --version all_sections_present_valid \
    --channel dev \
    --sign unsigned \
    --allow-partial >/dev/null

[[ -f "$POSITIVE" ]] || { echo "build did not produce $POSITIVE" >&2; exit 1; }

echo "2/3: Applying jq mutations to derive tampered variants..." >&2
jq '.artifacts[0].sha256 = "tampered000000000000000000000000000000000000000000000000000000"' \
  "$POSITIVE" > "$FIX/tampered_artifact_hash.json"
refresh_unsigned_canonical_sha "$FIX/tampered_artifact_hash.json"
jq '.signature.canonical_sha256 = "tampered000000000000000000000000000000000000000000000000000000"' \
  "$POSITIVE" > "$FIX/tampered_signature_canonical_hash.json"
jq '.artifacts |= map(select(.category != "doctrine/agents-md-counts"))' \
  "$POSITIVE" > "$FIX/missing_required_slot.json"
refresh_unsigned_canonical_sha "$FIX/missing_required_slot.json"
jq '.artifacts += [{"category":"unknown/forbidden","description":"injected","path":"fake","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size_bytes":0,"proof_categories":[]}]' \
  "$POSITIVE" > "$FIX/extra_unknown_slot.json"
refresh_unsigned_canonical_sha "$FIX/extra_unknown_slot.json"
jq '.artifacts[0].path = "/tmp/ft-attestation-outside.json"' \
  "$POSITIVE" > "$FIX/absolute_artifact_path.json"
refresh_unsigned_canonical_sha "$FIX/absolute_artifact_path.json"
jq '.artifacts[0].path = "../docs/attestations/schema.json"' \
  "$POSITIVE" > "$FIX/parent_artifact_path.json"
refresh_unsigned_canonical_sha "$FIX/parent_artifact_path.json"
jq '.schema_version = "9.9.9"' "$POSITIVE" > "$FIX/wrong_schema_version.json"
refresh_unsigned_canonical_sha "$FIX/wrong_schema_version.json"
jq 'del(.release)' "$POSITIVE" > "$FIX/missing_release_block.json"
refresh_unsigned_canonical_sha "$FIX/missing_release_block.json"
jq '.taxonomy_coverage = {"1":99,"7":99,"12":99}' \
  "$POSITIVE" > "$FIX/inflated_taxonomy_coverage.json"
refresh_unsigned_canonical_sha "$FIX/inflated_taxonomy_coverage.json"
jq '.confidence_summary = {"records":[],"best_confidence_by_category":[],"tampered":true}' \
  "$POSITIVE" > "$FIX/tampered_confidence_summary.json"
refresh_unsigned_canonical_sha "$FIX/tampered_confidence_summary.json"
# empty_bundle.json and malformed_json.json are bit-stable across regens
# (a 3-byte "{}" stub and a literal malformed string). Re-write to be safe.
echo '{}' > "$FIX/empty_bundle.json"
echo '{ this is not valid json' > "$FIX/malformed_json.json"

echo "Done. Verifying positive control..." >&2
verdict=$("${REPO_ROOT}/scripts/attestation-verify.sh" "$FIX/all_sections_present_valid.json" --json | jq -r '.ok')
if [[ "$verdict" == "true" ]]; then
  echo "OK: positive control verifies clean (ok=true)"
else
  echo "WARNING: positive control still reports ok=$verdict after regen" >&2
  exit 1
fi
