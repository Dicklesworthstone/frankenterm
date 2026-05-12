#!/usr/bin/env bash
# Regenerate the attestation verifier self-test corpus from the current dev bundle.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_BUNDLE="${FT_ATTESTATION_SELF_TEST_SOURCE:-${ROOT_DIR}/docs/attestations/0.0.0-dev.json}"
OUT_DIR="${FT_ATTESTATION_SELF_TEST_DIR:-${ROOT_DIR}/tests/attestation_verify_self_test}"
FIXTURE_DIR="${OUT_DIR}/fixtures"
EXPECTED_DIR="${OUT_DIR}/expected"
SIGNATURE_DIR="${OUT_DIR}/signatures"

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing required command: $cmd" >&2
    exit 1
  fi
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    shasum -a 256 "$file" | awk '{print $1}'
  fi
}

write_expected() {
  local name="$1"
  local expected_ok="$2"
  local regex="$3"
  local note="$4"
  jq -n \
    --arg fixture "tests/attestation_verify_self_test/fixtures/${name}.json" \
    --argjson expected_ok "$expected_ok" \
    --arg regex "$regex" \
    --arg note "$note" \
    '{
      fixture: $fixture,
      expected_ok: $expected_ok,
      expected_error_regex: $regex,
      flags: ["--strict-required", "--strict-deferred"],
      note: $note
    }' > "${EXPECTED_DIR}/${name}.json"
}

write_fixture() {
  local name="$1"
  local jq_filter="$2"
  local expected_ok="$3"
  local regex="$4"
  local note="$5"
  jq -S "$jq_filter" "$SOURCE_BUNDLE" > "${FIXTURE_DIR}/${name}.json"
  write_expected "$name" "$expected_ok" "$regex" "$note"
}

require_cmd jq
[[ -f "$SOURCE_BUNDLE" ]] || { echo "source bundle not found: $SOURCE_BUNDLE" >&2; exit 1; }

if ! "${ROOT_DIR}/scripts/attestation-verify.sh" "$SOURCE_BUNDLE" --json --strict-required --strict-deferred >/dev/null; then
  echo "source bundle must pass strict verification before rotating self-test fixtures: $SOURCE_BUNDLE" >&2
  exit 1
fi

mkdir -p "$FIXTURE_DIR" "$EXPECTED_DIR" "$SIGNATURE_DIR"

zero64="0000000000000000000000000000000000000000000000000000000000000000"
zero_sig="${SIGNATURE_DIR}/zero.ed25519.sig.hex"
fake_sigstore="${SIGNATURE_DIR}/fake.sigstore"
printf '%0128d\n' 0 > "$zero_sig"
printf '%s\n' '{"kind":"fake-sigstore-bundle","purpose":"attestation verifier self-test"}' > "$fake_sigstore"

schema_sha="$(sha256_file "${ROOT_DIR}/docs/attestations/schema.json")"
schema_size="$(wc -c < "${ROOT_DIR}/docs/attestations/schema.json" | tr -d ' ')"
fake_sigstore_sha="$(sha256_file "$fake_sigstore")"
fake_sigstore_size="$(wc -c < "$fake_sigstore" | tr -d ' ')"
wrong_public_key="1111111111111111111111111111111111111111111111111111111111111111"
tampered_public_key="2222222222222222222222222222222222222222222222222222222222222222"
zero_sig_rel="tests/attestation_verify_self_test/signatures/zero.ed25519.sig.hex"
fake_sigstore_rel="tests/attestation_verify_self_test/signatures/fake.sigstore"

write_fixture \
  "valid_baseline" \
  "." \
  true \
  "" \
  "Positive twin copied from the current strict-passing dev bundle."

write_fixture \
  "tampered_perf_slot" \
  "(.artifacts[] | select(.category == \"perf/headline-claims\") | .sha256) = \"${zero64}\"" \
  false \
  "artifact:docs/perf/headline-claims.json: sha256 mismatch" \
  "Perf headline-claims content hash is intentionally wrong."

write_fixture \
  "tampered_tui_slot" \
  "(.artifacts[] | select(.category == \"tui/render-parity\" and .path == \"docs/attestations/tui/render-parity.json\") | .sha256) = \"${zero64}\"" \
  false \
  "artifact:docs/attestations/tui/render-parity.json: sha256 mismatch" \
  "TUI render-parity content hash is intentionally wrong."

write_fixture \
  "tampered_security_slot" \
  "(.artifacts[] | select(.category == \"security/redactor-coverage\") | .sha256) = \"${zero64}\"" \
  false \
  "artifact:docs/security/redactor-coverage.json: sha256 mismatch" \
  "Security redactor-coverage content hash is intentionally wrong."

write_fixture \
  "tampered_doctrine_slot" \
  "(.artifacts[] | select(.category == \"doctrine/agents-md-counts\") | .sha256) = \"${zero64}\"" \
  false \
  "artifact:docs/attestations/doctrine/agents-md-counts.json: sha256 mismatch" \
  "Doctrine agents-md-counts content hash is intentionally wrong."

write_fixture \
  "tampered_proofs_slot" \
  "(.artifacts[] | select(.category == \"proofs/runtime-proof-trait\") | .sha256) = \"${zero64}\"" \
  false \
  "artifact:docs/attestations/proofs/runtime-proof-trait.json: sha256 mismatch" \
  "RuntimeProof artifact content hash is intentionally wrong."

write_fixture \
  "missing_required_slot" \
  "(.artifacts) |= map(select(.category != \"perf/headline-claims\"))" \
  false \
  "category:perf/headline-claims: no artifact found for required category" \
  "Required perf/headline-claims category is removed from the artifact list."

write_fixture \
  "extra_unknown_slot" \
  ".artifacts += [{
    category: \"unknown/not-real\",
    path: \"docs/attestations/schema.json\",
    media_type: \"application/json\",
    sha256: \"${schema_sha}\",
    size_bytes: ${schema_size},
    produced_by_bead: \"ft-tf6g3.39\",
    proof_categories: []
  }]" \
  false \
  "artifact_categories_declared: unknown artifact category/categories: unknown/not-real" \
  "Unknown artifact category is added with a real path and correct bytes."

write_fixture \
  "swapped_slot_order" \
  "(.artifacts[0] as \$first | .artifacts[1] as \$second | .artifacts[0] = \$second | .artifacts[1] = \$first)" \
  false \
  "canonical_sha256: expected" \
  "Artifact order is changed without recomputing the signing payload hash."

write_fixture \
  "signature_wrong_key" \
  ".signature = {
    method: \"ed25519\",
    canonical_sha256: .signature.canonical_sha256,
    signature_path: \"${zero_sig_rel}\",
    public_key: \"${wrong_public_key}\"
  }" \
  false \
  "signature: ed25519 verify failed" \
  "Bundle declares an Ed25519 key that cannot verify the committed signature bytes."

write_fixture \
  "signature_tampered" \
  ".signature = {
    method: \"ed25519\",
    canonical_sha256: .signature.canonical_sha256,
    signature_path: \"${zero_sig_rel}\",
    public_key: \"${tampered_public_key}\"
  }" \
  false \
  "signature: ed25519 verify failed" \
  "Bundle signature material is intentionally invalid."

write_fixture \
  "sigstore_expired_cert" \
  ".signature = {
    method: \"sigstore-cosign-keyless\",
    canonical_sha256: .signature.canonical_sha256,
    sigstore_bundle: {
      path: \"${fake_sigstore_rel}\",
      sha256: \"${fake_sigstore_sha}\",
      size_bytes: ${fake_sigstore_size}
    },
    certificate_identity: \"https://github.com/Dicklesworthstone/frankenterm/.github/workflows/release.yml@refs/tags/v0.0.0-expired\",
    certificate_oidc_issuer: \"https://token.actions.githubusercontent.com\"
  }" \
  false \
  "signature: (cosign not installed|cosign verify-blob failed)" \
  "Fake sigstore material represents the offline expired-certificate failure class."

write_fixture \
  "sigstore_wrong_identity" \
  ".signature = {
    method: \"sigstore-cosign-keyless\",
    canonical_sha256: .signature.canonical_sha256,
    sigstore_bundle: {
      path: \"${fake_sigstore_rel}\",
      sha256: \"${fake_sigstore_sha}\",
      size_bytes: ${fake_sigstore_size}
    },
    certificate_identity: \"https://github.com/attacker/not-frankenterm/.github/workflows/release.yml@refs/tags/v0.0.0\",
    certificate_oidc_issuer: \"https://token.actions.githubusercontent.com\"
  }" \
  false \
  "signature: (cosign not installed|cosign verify-blob failed)" \
  "Fake sigstore material represents the wrong-identity failure class."

fixture_count="$(find "$FIXTURE_DIR" -maxdepth 1 -name '*.json' | wc -l | tr -d ' ')"
expected_count="$(find "$EXPECTED_DIR" -maxdepth 1 -name '*.json' | wc -l | tr -d ' ')"
echo "rotated attestation verifier self-test corpus"
echo "  source   : $SOURCE_BUNDLE"
echo "  fixtures : $fixture_count"
echo "  expected : $expected_count"
