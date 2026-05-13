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
#   1. Build a fresh self-test bundle from current HEAD via
#      scripts/attestation-build.sh --version 0.0.0-self-test
#      --channel dev --sign unsigned --allow-partial.
#   2. Copy it as the positive-control fixture.
#   3. Apply the 8 documented jq mutations to derive the tampered variants.
#   4. Delete the transient docs/attestations/0.0.0-self-test.json (it's
#      not a release artifact; only the fixture copies are durable).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CANON="${REPO_ROOT}/docs/attestations/0.0.0-self-test.json"
FIX="${REPO_ROOT}/tests/fixtures/attestation-verify-self-test/fixtures"

command -v jq >/dev/null || { echo "jq required" >&2; exit 2; }
[[ -x "${REPO_ROOT}/scripts/attestation-build.sh" ]] || {
  echo "attestation-build.sh not executable" >&2; exit 2;
}
[[ -d "$FIX" ]] || { echo "$FIX missing" >&2; exit 2; }

cd "$REPO_ROOT"

echo "1/4: Building fresh 0.0.0-self-test bundle from current HEAD..." >&2
./scripts/attestation-build.sh --version 0.0.0-self-test --channel dev --sign unsigned --allow-partial >/dev/null

[[ -f "$CANON" ]] || { echo "build did not produce $CANON" >&2; exit 1; }

echo "2/4: Copying positive control..." >&2
cp "$CANON" "$FIX/all_sections_present_valid.json"

echo "3/4: Applying 8 jq mutations to derive tampered variants..." >&2
jq '.artifacts[0].sha256 = "tampered000000000000000000000000000000000000000000000000000000"' \
  "$CANON" > "$FIX/tampered_artifact_hash.json"
jq '.signature.canonical_sha256 = "tampered000000000000000000000000000000000000000000000000000000"' \
  "$CANON" > "$FIX/tampered_signature_canonical_hash.json"
jq 'del(.artifacts[0])' "$CANON" > "$FIX/missing_required_slot.json"
jq '.artifacts += [{"category":"unknown/forbidden","description":"injected","path":"fake","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size_bytes":0,"proof_categories":[]}]' \
  "$CANON" > "$FIX/extra_unknown_slot.json"
jq '.schema_version = "9.9.9"' "$CANON" > "$FIX/wrong_schema_version.json"
jq 'del(.release)' "$CANON" > "$FIX/missing_release_block.json"
jq '.taxonomy_coverage = {"1":99,"7":99,"12":99}' \
  "$CANON" > "$FIX/inflated_taxonomy_coverage.json"
jq '.confidence_summary = {"records":[],"best_confidence_by_category":[],"tampered":true}' \
  "$CANON" > "$FIX/tampered_confidence_summary.json"
# empty_bundle.json and malformed_json.json are bit-stable across regens
# (a 3-byte "{}" stub and a literal malformed string). Re-write to be safe.
echo '{}' > "$FIX/empty_bundle.json"
echo '{ this is not valid json' > "$FIX/malformed_json.json"

echo "4/4: Removing transient bundle..." >&2
rm -f "$CANON"

echo "Done. Verifying positive control..." >&2
verdict=$("${REPO_ROOT}/scripts/attestation-verify.sh" "$FIX/all_sections_present_valid.json" --json | jq -r '.ok')
if [[ "$verdict" == "true" ]]; then
  echo "OK: positive control verifies clean (ok=true)"
else
  echo "WARNING: positive control still reports ok=$verdict after regen" >&2
  exit 1
fi
