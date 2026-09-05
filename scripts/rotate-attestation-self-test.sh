#!/usr/bin/env bash
# Build a retained, fixture-only verifier corpus; never refresh production goldens.
set -euo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
OUT_DIR="${FT_ATTESTATION_SELF_TEST_DIR:-}"
SOURCE_BUNDLE="${FT_ATTESTATION_SELF_TEST_SOURCE:-}"
SOURCE_MANIFEST="${FT_ATTESTATION_SELF_TEST_MANIFEST:-}"
# Fixture verification must not inherit operator trust, retractions or schema overrides.
unset FT_ATTESTATION_RELEASE_POLICY FT_ATTESTATION_PRIOR_BUNDLE FT_ATTESTATION_SCHEMA FT_PROOF_TAXONOMY

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
    --arg fixture "${FIXTURE_DIR#"$ROOT_DIR"/}/${name}.json" \
    --arg manifest "${MANIFEST#"$ROOT_DIR"/}" \
    --arg tools "${TOOLS_DIR#"$ROOT_DIR"/}" \
    --arg retractions "${FT_ATTESTATION_RETRACTIONS_ROOT#"$ROOT_DIR"/}" \
    --argjson expected_ok "$expected_ok" \
    --arg regex "$regex" \
    --arg note "$note" \
    '{
      fixture_only: true,
      fixture: $fixture,
      manifest: $manifest,
      tools: $tools,
      retractions: $retractions,
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
  local category="${6:-}"
  jq -S --arg category "$category" --arg zero64 "$zero64" \
    --arg zero_sig "$zero_sig_rel" --arg fake_sigstore "$fake_sigstore_rel" \
    "$jq_filter" "$SOURCE_BUNDLE" > "${FIXTURE_DIR}/${name}.json"
  write_expected "$name" "$expected_ok" "$regex" "$note"
}

require_cmd jq

ARTIFACT_ROOT="${ROOT_DIR}/target/test-artifacts"
mkdir -p "$ARTIFACT_ROOT"
ARTIFACT_ROOT="$(cd "$ARTIFACT_ROOT" && pwd -P)"
[[ "$ARTIFACT_ROOT" == "$ROOT_DIR/"* ]] || { echo "test-artifact root escapes repository" >&2; exit 2; }
if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$(mktemp -d "$ARTIFACT_ROOT/attestation-verifier.XXXXXX")"
else
  [[ "$OUT_DIR" == /* ]] || OUT_DIR="$ROOT_DIR/$OUT_DIR"
  out_parent="$(cd "$(dirname "$OUT_DIR")" && pwd -P)"
  out_leaf="$(basename "$OUT_DIR")"
  [[ "$out_parent/" == "$ARTIFACT_ROOT/"* && "$out_leaf" =~ ^[A-Za-z0-9._-]+$ && "$out_leaf" != . && "$out_leaf" != .. ]] || {
    echo "output must be a fresh directory beneath target/test-artifacts" >&2; exit 2;
  }
  OUT_DIR="$out_parent/$out_leaf"
  mkdir "$OUT_DIR" || { echo "refusing to reuse self-test evidence: $OUT_DIR" >&2; exit 2; }
fi
FIXTURE_DIR="$OUT_DIR/fixtures"
EXPECTED_DIR="$OUT_DIR/expected"
SIGNATURE_DIR="$OUT_DIR/signatures"
SOURCE_BUILD_DIR="$OUT_DIR/source"
TOOLS_DIR="$OUT_DIR/tools"
MANIFEST="$OUT_DIR/manifest.json"
export FT_ATTESTATION_MANIFEST="$MANIFEST"
export FT_ATTESTATION_RETRACTIONS_ROOT="$OUT_DIR/retractions"
export TMPDIR="$OUT_DIR/crypto"
mkdir "$FIXTURE_DIR" "$EXPECTED_DIR" "$SIGNATURE_DIR" "$SOURCE_BUILD_DIR" "$TOOLS_DIR" "$FT_ATTESTATION_RETRACTIONS_ROOT" "$TMPDIR"
echo "Retained fixture-only corpus: $OUT_DIR" >&2

# Real local tools only. Excluding cosign deliberately exercises the verifier's
# unavailable-tool negative, without permitting a trust-root/network fetch.
# OpenSSL still verifies the malformed Ed25519 signatures offline.
for tool in bash jq git awk shasum openssl xxd tr find sort wc grep date mkdir basename dirname cat cp mktemp env; do
  require_cmd "$tool"
  tool_path="$(command -v "$tool")"
  [[ "$tool_path" == /* && -f "$tool_path" && -x "$tool_path" ]] || { echo "tool must resolve to an absolute executable: $tool" >&2; exit 2; }
  ln -s "$tool_path" "$TOOLS_DIR/$tool"
done
export PATH="$TOOLS_DIR"

if [[ -z "$SOURCE_BUNDLE" ]]; then
  [[ -z "$SOURCE_MANIFEST" ]] || { echo "custom manifest requires an explicit source bundle" >&2; exit 2; }
  jq -n --arg root "${SOURCE_BUILD_DIR#"$ROOT_DIR"/}" '{
    fixture_only: true,
    required_categories: ["perf/headline-claims", "tui/render-parity", "security/redactor-coverage", "doctrine/agents-md-counts", "proofs/runtime-proof-trait"],
    slots: (["perf/headline-claims", "tui/render-parity", "security/redactor-coverage", "doctrine/agents-md-counts", "proofs/runtime-proof-trait"]
      | to_entries | map({category:.value, path:($root + "/artifact-" + (.key|tostring) + ".json"),
          media_type:"application/json", produced_by_bead:"ft-xxfwy.49", proof_categories:[1],
          description:"Fixture only: exercises verifier integrity, not product capability."}))
  }' > "$MANIFEST"
  for index in 0 1 2 3 4; do
    jq -n --argjson index "$index" '{fixture_only:true, fixture_index:$index}' > "$SOURCE_BUILD_DIR/artifact-$index.json"
  done
  if ! FT_ATTESTATION_OUT_DIR="$SOURCE_BUILD_DIR" \
    "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version 0.0.0-verifier-fixture \
      --channel dev \
      --sign unsigned \
      --strict-deferred > "$OUT_DIR/build.stdout" 2> "$OUT_DIR/build.stderr"; then
    cat "$OUT_DIR/build.stdout" "$OUT_DIR/build.stderr" >&2
    exit 1
  fi
  SOURCE_BUNDLE="$SOURCE_BUILD_DIR/0.0.0-verifier-fixture.json"
else
  [[ -f "$SOURCE_MANIFEST" ]] || { echo "custom source requires FT_ATTESTATION_SELF_TEST_MANIFEST" >&2; exit 2; }
  cp "$SOURCE_MANIFEST" "$MANIFEST"
fi

[[ -f "$SOURCE_BUNDLE" ]] || { echo "source bundle not found: $SOURCE_BUNDLE" >&2; exit 1; }

if ! "${ROOT_DIR}/scripts/attestation-verify.sh" "$SOURCE_BUNDLE" --json --strict-required --strict-deferred > "$OUT_DIR/source-verification.json" 2> "$OUT_DIR/source-verification.stderr"; then
  cat "$OUT_DIR/source-verification.json" "$OUT_DIR/source-verification.stderr" >&2
  echo "source bundle must pass strict verification before rotating self-test fixtures: $SOURCE_BUNDLE" >&2
  exit 1
fi

zero64="0000000000000000000000000000000000000000000000000000000000000000"
zero_sig="${SIGNATURE_DIR}/zero.ed25519.sig.hex"
fake_sigstore="${SIGNATURE_DIR}/fake.sigstore"
printf '%0128d\n' 0 > "$zero_sig"
printf '%s\n' '{"kind":"fake-sigstore-bundle","purpose":"attestation verifier self-test"}' > "$fake_sigstore"

fake_sigstore_sha="$(sha256_file "$fake_sigstore")"
fake_sigstore_size="$(wc -c < "$fake_sigstore" | tr -d ' ')"
wrong_public_key="1111111111111111111111111111111111111111111111111111111111111111"
tampered_public_key="2222222222222222222222222222222222222222222222222222222222222222"
zero_sig_rel="${zero_sig#"$ROOT_DIR"/}"
fake_sigstore_rel="${fake_sigstore#"$ROOT_DIR"/}"

write_fixture \
  "valid_baseline" \
  "." \
  true \
  "" \
  "Fixture-only positive twin from the owned strict-passing source; no production proof."

write_hash_fixture() {
  local name="$1" category="$2" artifact_path error_regex
  artifact_path="$(jq -er --arg category "$category" '
    [.artifacts[] | select(.category == $category)][0].path | select(type == "string" and length > 0)
  ' "$SOURCE_BUNDLE")" || { echo "source lacks tamper category: $category" >&2; exit 2; }
  error_regex="$(jq -nr --arg value "artifact:$artifact_path: sha256 mismatch" \
    '$value | gsub("(?<c>[][(){}.^$*+?|\\\\])"; "\\\(.c)")')"
  # jq binds these variables through --arg; shell expansion is not intended.
  # shellcheck disable=SC2016
  write_fixture "$name" \
    '(.artifacts[] | select(.category == $category) | .sha256) = $zero64' \
    false "$error_regex" "Fixture-only $category hash is intentionally wrong." "$category"
}

write_hash_fixture tampered_perf_slot perf/headline-claims
write_hash_fixture tampered_tui_slot tui/render-parity
write_hash_fixture tampered_security_slot security/redactor-coverage
write_hash_fixture tampered_doctrine_slot doctrine/agents-md-counts
write_hash_fixture tampered_proofs_slot proofs/runtime-proof-trait

write_fixture \
  "missing_required_slot" \
  "(.artifacts) |= map(select(.category != \"perf/headline-claims\"))" \
  false \
  "category:perf/headline-claims: no artifact found for required category" \
  "Required perf/headline-claims category is removed from the artifact list."

write_fixture \
  "extra_unknown_slot" \
  '.artifacts += [(.artifacts[0] | .category = "unknown/not-real" | .proof_categories = [])]' \
  false \
  "artifact_categories_declared: unknown artifact category/categories: unknown/not-real" \
  "Unknown artifact category is added with a real path and correct bytes."

write_fixture \
  "absolute_artifact_path" \
  "(.artifacts[0].path) = \"/tmp/ft-attestation-outside.json\"" \
  false \
  "artifact:/tmp/ft-attestation-outside.json: artifact path must be repo-relative without parent traversal" \
  "Absolute artifact paths must be rejected before disk reads."

write_fixture \
  "parent_artifact_path" \
  "(.artifacts[0].path) = \"../docs/attestations/schema.json\"" \
  false \
  "artifact:../docs/attestations/schema.json: artifact path must be repo-relative without parent traversal" \
  "Parent-traversal artifact paths must be rejected before disk reads."

write_fixture \
  "dot_segment_artifact_path" \
  "(.artifacts[0].path) = \"./docs/attestations/schema.json\"" \
  false \
  "artifact:./docs/attestations/schema.json: artifact path must be repo-relative without parent traversal" \
  "Dot-segment artifact paths must be rejected before disk reads."

write_fixture \
  "empty_segment_artifact_path" \
  "(.artifacts[0].path) = \"docs//attestations/schema.json\"" \
  false \
  "artifact:docs//attestations/schema.json: artifact path must be repo-relative without parent traversal" \
  "Empty-segment artifact paths must be rejected before disk reads."

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
    signature_path: \$zero_sig,
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
    signature_path: \$zero_sig,
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
      path: \$fake_sigstore,
      sha256: \"${fake_sigstore_sha}\",
      size_bytes: ${fake_sigstore_size}
    },
    certificate_identity: \"https://fixture.invalid/expired-signer\",
    certificate_oidc_issuer: \"https://issuer.fixture.invalid\"
  }" \
  false \
  "signature: (cosign not installed|cosign verify-blob failed)" \
  "Fixture only: unavailable cosign rejection; this does not validate certificate expiry."

write_fixture \
  "sigstore_wrong_identity" \
  ".signature = {
    method: \"sigstore-cosign-keyless\",
    canonical_sha256: .signature.canonical_sha256,
    sigstore_bundle: {
      path: \$fake_sigstore,
      sha256: \"${fake_sigstore_sha}\",
      size_bytes: ${fake_sigstore_size}
    },
    certificate_identity: \"https://attacker.fixture.invalid/not-frankenterm\",
    certificate_oidc_issuer: \"https://issuer.fixture.invalid\"
  }" \
  false \
  "signature: (cosign not installed|cosign verify-blob failed)" \
  "Fixture only: unavailable cosign rejection; this does not validate OIDC identity."

fixture_count="$(find "$FIXTURE_DIR" -maxdepth 1 -name '*.json' | wc -l | tr -d ' ')"
expected_count="$(find "$EXPECTED_DIR" -maxdepth 1 -name '*.json' | wc -l | tr -d ' ')"
echo "rotated attestation verifier self-test corpus"
echo "  source   : $SOURCE_BUNDLE"
echo "  fixtures : $fixture_count"
echo "  expected : $expected_count"
echo "  manifest : $MANIFEST"
echo "  expected directory : $EXPECTED_DIR"
