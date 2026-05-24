#!/usr/bin/env bash
# ft-e87u6.2 -- attestation-build deferred-slot behavior matrix.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_ROOT="${FT_E87U6_2_ARTIFACT_DIR:-${ROOT_DIR}/target/test-artifacts/ft-e87u6-2-attestation-build-matrix-${RUN_ID}}"
mkdir -p "${ARTIFACT_ROOT}"

SUMMARY_FILE="${ARTIFACT_ROOT}/summary.json"
RESULTS_JSONL="${ARTIFACT_ROOT}/results.jsonl"
: > "${RESULTS_JSONL}"

pass=0
fail=0
total=0

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${file}" | awk '{print $1}'
  else
    shasum -a 256 "${file}" | awk '{print $1}'
  fi
}

sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

install_fake_cosign() {
  local fake_bin="$1"
  mkdir -p "${fake_bin}"
  cat >"${fake_bin}/cosign" <<'EOS'
#!/usr/bin/env bash
set -euo pipefail
cmd="${1:-}"
shift || true
case "${cmd}" in
  sign-blob)
    bundle=""
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --yes) shift ;;
        --bundle) bundle="${2:?--bundle requires a path}"; shift 2 ;;
        *) shift ;;
      esac
    done
    [[ -n "${bundle}" ]] || { echo "fake cosign: missing --bundle" >&2; exit 2; }
    mkdir -p "$(dirname "${bundle}")"
    jq -n '{
      mediaType: "application/vnd.dev.sigstore.bundle.v0.3+json",
      verificationMaterial: {
        certificate: {rawBytes: "ZmFrZS1mdWxjaW8tY2VydA=="},
        tlogEntries: [{
          logIndex: "1",
          inclusionPromise: {signedEntryTimestamp: "ZmFrZS1zZXQ="},
          inclusionProof: {
            logIndex: "1",
            rootHash: "ZmFrZQ==",
            treeSize: "1",
            hashes: [],
            checkpoint: {envelope: "rekor.sigstore.dev fake checkpoint"}
          }
        }]
      },
      messageSignature: {
        messageDigest: {algorithm: "SHA2_256", digest: "ZmFrZS1kaWdlc3Q="},
        signature: "ZmFrZS1zaWduYXR1cmU="
      }
    }' >"${bundle}"
    ;;
  verify-blob)
    bundle=""
    blob=""
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --bundle) bundle="${2:?--bundle requires a path}"; shift 2 ;;
        --certificate-identity|--certificate-oidc-issuer) shift 2 ;;
        *) blob="$1"; shift ;;
      esac
    done
    [[ -f "${bundle}" && -f "${blob}" ]] || exit 1
    ;;
  *)
    echo "fake cosign: unsupported command ${cmd}" >&2
    exit 2
    ;;
esac
EOS
  chmod +x "${fake_bin}/cosign"
}

json_slot() {
  local path_json="$1"
  local deferred_to_bead="$2"
  local deferred_reason="$3"
  jq -cn \
    --argjson path "${path_json}" \
    --arg deferred_to_bead "${deferred_to_bead}" \
    --arg deferred_reason "${deferred_reason}" \
    '{
      category: "perf/headline-claims",
      path: $path,
      media_type: "application/json",
      produced_by_bead: "ft-syqcz.3",
      proof_categories: [5],
      description: "matrix test slot"
    }
    + (if $deferred_to_bead == "" then {} else {deferred_to_bead: $deferred_to_bead} end)
    + (if $deferred_reason == "" then {} else {deferred_reason: $deferred_reason} end)'
}

write_manifest() {
  local manifest="$1"
  local slot="$2"
  jq -n --argjson slot "${slot}" '{
    "$schema": "./schema.json#/$defs/manifestPlaceholder",
    required_categories: ["perf/headline-claims"],
    slots: [$slot]
  }' > "${manifest}"
}

record_result() {
  local name="$1"
  local expected="$2"
  local actual="$3"
  local outcome="$4"
  local out_dir="$5"
  jq -cn \
    --arg name "${name}" \
    --arg expected "${expected}" \
    --arg actual "${actual}" \
    --arg outcome "${outcome}" \
    --arg out_dir "${out_dir}" \
    '{name:$name, expected_exit:$expected, actual_exit:$actual, outcome:$outcome, artifact_dir:$out_dir}' \
    >> "${RESULTS_JSONL}"
}

run_case() {
  local name="$1"
  local expected_rc="$2"
  local path_json="$3"
  local deferred_to_bead="$4"
  local deferred_reason="$5"
  local expected_deferred_count="$6"
  shift 6

  local case_dir="${ARTIFACT_ROOT}/${name}"
  local manifest="${case_dir}/manifest.json"
  local out_dir="${case_dir}/out"
  local stdout_file="${case_dir}/stdout.txt"
  local stderr_file="${case_dir}/stderr.txt"
  local version="0.0.0-${name//_/-}"
  local rc=0
  mkdir -p "${case_dir}" "${out_dir}"
  write_manifest "${manifest}" "$(json_slot "${path_json}" "${deferred_to_bead}" "${deferred_reason}")"

  set +e
  FT_ATTESTATION_MANIFEST="${manifest}" \
  FT_ATTESTATION_OUT_DIR="${out_dir}" \
  FT_BEAD_ID="ft-e87u6.2" \
  FT_SCENARIO_ID="attestation_build_matrix_${name}" \
  FT_CORRELATION_ID="ft-e87u6.2-${RUN_ID}-${name}" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${version}" \
      --channel dev \
      --sign unsigned \
      "$@" > "${stdout_file}" 2> "${stderr_file}"
  rc=$?
  set -e

  total=$((total + 1))
  if [[ "${rc}" != "${expected_rc}" ]]; then
    fail=$((fail + 1))
    record_result "${name}" "${expected_rc}" "${rc}" "failed" "${case_dir}"
    echo "FAIL ${name}: expected rc ${expected_rc}, got ${rc}" >&2
    return 0
  fi

  if [[ "${expected_rc}" == "0" ]]; then
    local bundle="${out_dir}/${version}.json"
    if [[ ! -f "${bundle}" ]]; then
      fail=$((fail + 1))
      record_result "${name}" "${expected_rc}" "${rc}" "failed" "${case_dir}"
      echo "FAIL ${name}: expected bundle ${bundle}" >&2
      return 0
    fi
    local deferred_count
    deferred_count="$(jq '(.deferred_slots // []) | length' "${bundle}")"
    if [[ "${deferred_count}" != "${expected_deferred_count}" ]]; then
      fail=$((fail + 1))
      record_result "${name}" "${expected_rc}" "${rc}" "failed" "${case_dir}"
      echo "FAIL ${name}: expected ${expected_deferred_count} deferred slots, got ${deferred_count}" >&2
      return 0
    fi
    local confidence_schema_path
    confidence_schema_path="$(jq -r '.confidence_summary.schema_path // ""' "${bundle}")"
    if [[ "${confidence_schema_path}" != "docs/proofs/confidence-format-schema.json" ]]; then
      fail=$((fail + 1))
      record_result "${name}" "${expected_rc}" "${rc}" "failed" "${case_dir}"
      echo "FAIL ${name}: missing confidence_summary schema path" >&2
      return 0
    fi
  fi

  if [[ "${name}" == deferred_* ]]; then
    if ! grep -q '^\[build:json\]' "${stderr_file}"; then
      fail=$((fail + 1))
      record_result "${name}" "${expected_rc}" "${rc}" "failed" "${case_dir}"
      echo "FAIL ${name}: missing structured [build:json] event" >&2
      return 0
    fi
  fi

  pass=$((pass + 1))
  record_result "${name}" "${expected_rc}" "${rc}" "passed" "${case_dir}"
  echo "PASS ${name}"
}

run_sigstore_cases() {
  local fake_bin="${ARTIFACT_ROOT}/fake-cosign-bin"
  install_fake_cosign "${fake_bin}"

  local build_name="cosign_build_records_sigstore_hash"
  local build_dir="${ARTIFACT_ROOT}/${build_name}"
  local manifest="${build_dir}/manifest.json"
  local out_dir="${build_dir}/out"
  local version="0.0.0-cosign-metadata"
  local rc=0
  mkdir -p "${build_dir}" "${out_dir}"
  write_manifest "${manifest}" "$(json_slot '"docs/attestations/schema.json"' "" "")"

  set +e
  PATH="${fake_bin}:$PATH" \
  COSIGN_IDENTITY="https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v${version}" \
  COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com" \
  FT_ATTESTATION_MANIFEST="${manifest}" \
  FT_ATTESTATION_OUT_DIR="${out_dir}" \
  FT_BEAD_ID="ft-tf6g3.22" \
  FT_SCENARIO_ID="attestation_sigstore_metadata_build" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${version}" \
      --channel stable \
      --sign cosign >"${build_dir}/stdout.txt" 2>"${build_dir}/stderr.txt"
  rc=$?
  set -e

  total=$((total + 1))
  if [[ "${rc}" != "0" ]]; then
    fail=$((fail + 1))
    record_result "${build_name}" "0" "${rc}" "failed" "${build_dir}"
    echo "FAIL ${build_name}: build failed" >&2
    return 0
  fi
  local built_bundle="${out_dir}/${version}.json"
  local built_sigstore="${out_dir}/${version}.sigstore"
  local recorded_path recorded_hash recorded_size actual_path actual_hash actual_size
  recorded_path="$(jq -r '.signature.sigstore_bundle.path // ""' "${built_bundle}")"
  recorded_hash="$(jq -r '.signature.sigstore_bundle.sha256 // ""' "${built_bundle}")"
  recorded_size="$(jq -r '.signature.sigstore_bundle.size_bytes // ""' "${built_bundle}")"
  actual_path="${built_sigstore#"${ROOT_DIR}/"}"
  actual_hash="$(sha256_file "${built_sigstore}")"
  actual_size="$(wc -c < "${built_sigstore}" | tr -d ' ')"
  if [[ "${recorded_path}" != "${actual_path}" || "${recorded_hash}" != "${actual_hash}" || "${recorded_size}" != "${actual_size}" ]]; then
    fail=$((fail + 1))
    record_result "${build_name}" "0" "metadata_mismatch" "failed" "${build_dir}"
    echo "FAIL ${build_name}: sigstore path/hash/size metadata mismatch" >&2
    return 0
  fi
  pass=$((pass + 1))
  record_result "${build_name}" "0" "0" "passed" "${build_dir}"
  echo "PASS ${build_name}"

  local verify_name="verify_checks_sigstore_hash_before_cosign"
  local verify_dir="${ARTIFACT_ROOT}/${verify_name}"
  local sigstore_path="docs/attestations/schema.json"
  local sigstore_hash sigstore_size no_sig canonical_payload canonical_sha verify_bundle bad_bundle bad_confidence_bundle
  mkdir -p "${verify_dir}"
  sigstore_hash="$(sha256_file "${ROOT_DIR}/${sigstore_path}")"
  sigstore_size="$(wc -c < "${ROOT_DIR}/${sigstore_path}" | tr -d ' ')"
  no_sig="$(jq -n '{
    schema_version: "1.0.0",
    release: {version: "0.2.0", tag: "v0.2.0", channel: "stable"},
    generated_at: "2026-05-12T00:00:00Z",
    generator: {name: "scripts/attestation-build.sh", version: "1.4.0"},
    git: {
      commit: "0123456789abcdef0123456789abcdef01234567",
      tree: "89abcdef0123456789abcdef0123456789abcdef",
      branch: "main"
    },
    artifacts: [{
      category: "doctrine/agents-md-counts",
      path: "docs/attestations/schema.json",
      media_type: "application/json",
      sha256: "",
      size_bytes: 0,
      proof_categories: [5]
    }],
    required_categories: ["doctrine/agents-md-counts"],
    deferred_slots: [],
    taxonomy_coverage: {
      schema_version: "1.0.0",
      taxonomy_path: "docs/proof-taxonomy.json",
      category_counts: ([range(1; 12) as $id | {
        id: $id,
        slug: (if $id == 5 then "quantitative-attestation" else "category-\($id)" end),
        name: (if $id == 5 then "Quantitative Attestation" else "Category \($id)" end),
        bridge_plan_core: ($id <= 10),
        artifact_count: (if $id == 5 then 1 else 0 end),
        deferred_slot_count: 0,
        below_threshold: ($id != 5)
      }]),
      below_threshold_count: 10,
      uncategorized_artifact_count: 0,
      delta_from_prior_release: {
        status: "no_prior_bundle",
        category_deltas: ([range(1; 12) as $id | {
          id: $id,
          slug: (if $id == 5 then "quantitative-attestation" else "category-\($id)" end),
          artifact_delta: (if $id == 5 then 1 else 0 end),
          deferred_slot_delta: 0
        }])
      }
    },
    confidence_summary: {
      schema_version: "1.0.0",
      schema_path: "docs/proofs/confidence-format-schema.json",
      records: [{
        proof_id: "release-bundle.quantitative-attestation.best-confidence",
        proof_category: 5,
        claim: "Best available confidence for Quantitative Attestation in this release bundle.",
        confidence_type: "frequentist",
        confidence_value: {
          status: "not_quantified",
          reason: "Source artifact is attested by hash but does not yet publish a canonical numeric confidence record."
        },
        sample_size_or_state_count: {
          kind: "artifact_count",
          value: 1,
          unit: "delivered_artifacts"
        },
        time_budget_consumed: {
          seconds: 0,
          budget_seconds: null,
          status: "not_reported"
        },
        methodology_url: "docs/proof-taxonomy.json#quantitative-attestation",
        source_artifact_hash: "",
        source_artifact_path: "docs/attestations/schema.json"
      }],
      best_confidence_by_category: [{
        proof_id: "release-bundle.quantitative-attestation.best-confidence",
        proof_category: 5,
        claim: "Best available confidence for Quantitative Attestation in this release bundle.",
        confidence_type: "frequentist",
        confidence_value: {
          status: "not_quantified",
          reason: "Source artifact is attested by hash but does not yet publish a canonical numeric confidence record."
        },
        sample_size_or_state_count: {
          kind: "artifact_count",
          value: 1,
          unit: "delivered_artifacts"
        },
        time_budget_consumed: {
          seconds: 0,
          budget_seconds: null,
          status: "not_reported"
        },
        methodology_url: "docs/proof-taxonomy.json#quantitative-attestation",
        source_artifact_hash: "",
        source_artifact_path: "docs/attestations/schema.json"
      }]
    }
  }')"
  no_sig="$(jq \
    --arg artifact_sha "${sigstore_hash}" \
    --argjson artifact_size "${sigstore_size}" \
    '.artifacts[0].sha256 = $artifact_sha
     | .artifacts[0].size_bytes = $artifact_size
     | .confidence_summary.records[0].source_artifact_hash = $artifact_sha
     | .confidence_summary.best_confidence_by_category[0].source_artifact_hash = $artifact_sha' \
    <<<"${no_sig}")"
  canonical_payload="$(jq -S -c '.' <<<"${no_sig}")"
  canonical_sha="$(printf '%s' "${canonical_payload}" | sha256_stdin)"
  verify_bundle="${verify_dir}/sigstore-valid.json"
  bad_bundle="${verify_dir}/sigstore-bad-hash.json"
  jq \
    --arg canonical_sha "${canonical_sha}" \
    --arg sigstore_path "${sigstore_path}" \
    --arg sigstore_hash "${sigstore_hash}" \
    --argjson sigstore_size "${sigstore_size}" \
    '. + {signature: {
      method: "sigstore-cosign-keyless",
      canonical_sha256: $canonical_sha,
      sigstore_bundle: {path: $sigstore_path, sha256: $sigstore_hash, size_bytes: $sigstore_size},
      certificate_identity: "https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v0.2.0",
      certificate_oidc_issuer: "https://token.actions.githubusercontent.com"
    }}' <<<"${no_sig}" >"${verify_bundle}"
  jq '.signature.sigstore_bundle.sha256 = "0000000000000000000000000000000000000000000000000000000000000000"' \
    "${verify_bundle}" >"${bad_bundle}"
  bad_confidence_bundle="${verify_dir}/sigstore-bad-confidence.json"
  jq 'del(.confidence_summary.records[0].source_artifact_hash)' \
    "${verify_bundle}" >"${bad_confidence_bundle}"

  set +e
  PATH="${fake_bin}:$PATH" bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${verify_bundle}" >"${verify_dir}/valid.out" 2>&1
  rc=$?
  set -e
  total=$((total + 1))
  if [[ "${rc}" != "0" ]]; then
    fail=$((fail + 1))
    record_result "${verify_name}" "0" "${rc}" "failed" "${verify_dir}"
    echo "FAIL ${verify_name}: valid sigstore bundle did not verify" >&2
    return 0
  fi

  set +e
  PATH="${fake_bin}:$PATH" bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${bad_bundle}" >"${verify_dir}/bad.out" 2>&1
  rc=$?
  set -e
  if [[ "${rc}" == "0" ]] || ! grep -q "sigstore_bundle" "${verify_dir}/bad.out"; then
    fail=$((fail + 1))
    record_result "${verify_name}" "hash_mismatch_failure" "${rc}" "failed" "${verify_dir}"
    echo "FAIL ${verify_name}: bad sigstore hash was not rejected" >&2
    return 0
  fi

  set +e
  PATH="${fake_bin}:$PATH" bash "${ROOT_DIR}/scripts/attestation-verify.sh" "${bad_confidence_bundle}" >"${verify_dir}/bad-confidence.out" 2>&1
  rc=$?
  set -e
  if [[ "${rc}" == "0" ]] || ! grep -q "confidence_summary" "${verify_dir}/bad-confidence.out"; then
    fail=$((fail + 1))
    record_result "${verify_name}" "confidence_failure" "${rc}" "failed" "${verify_dir}"
    echo "FAIL ${verify_name}: malformed confidence record was not rejected" >&2
    return 0
  fi

  pass=$((pass + 1))
  record_result "${verify_name}" "hash_and_confidence_failures" "${rc}" "passed" "${verify_dir}"
  echo "PASS ${verify_name}"
}

run_external_signed_output_case() {
  local fake_bin="${ARTIFACT_ROOT}/fake-cosign-external-bin"
  install_fake_cosign "${fake_bin}"

  local name="signed_output_outside_repo_fails"
  local case_dir="${ARTIFACT_ROOT}/${name}"
  local manifest="${case_dir}/manifest.json"
  local external_out_dir="${TMPDIR:-/tmp}/ft-e87u6-2-external-signed-output-${RUN_ID}"
  local version="0.0.0-external-signed-output"
  local rc=0
  mkdir -p "${case_dir}" "${external_out_dir}"
  write_manifest "${manifest}" "$(json_slot '"docs/attestations/schema.json"' "" "")"

  set +e
  PATH="${fake_bin}:$PATH" \
  COSIGN_IDENTITY="https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v${version}" \
  COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com" \
  FT_ATTESTATION_MANIFEST="${manifest}" \
  FT_ATTESTATION_OUT_DIR="${external_out_dir}" \
  FT_BEAD_ID="ft-e87u6.2" \
  FT_SCENARIO_ID="attestation_external_signed_output_guard" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${version}" \
      --channel stable \
      --sign cosign >"${case_dir}/stdout.txt" 2>"${case_dir}/stderr.txt"
  rc=$?
  set -e

  total=$((total + 1))
  if [[ "${rc}" == "0" ]] || ! grep -q "signed attestation output must be inside the repository" "${case_dir}/stderr.txt"; then
    fail=$((fail + 1))
    record_result "${name}" "nonzero_external_output_rejection" "${rc}" "failed" "${case_dir}"
    echo "FAIL ${name}: external signed output was not rejected" >&2
    return 0
  fi

  pass=$((pass + 1))
  record_result "${name}" "nonzero_external_output_rejection" "${rc}" "passed" "${case_dir}"
  echo "PASS ${name}"
}

run_parent_signed_output_case() {
  local fake_bin="${ARTIFACT_ROOT}/fake-cosign-parent-bin"
  install_fake_cosign "${fake_bin}"

  local name="signed_output_parent_traversal_fails"
  local case_dir="${ARTIFACT_ROOT}/${name}"
  local manifest="${case_dir}/manifest.json"
  local parent_out_dir="${case_dir}/out-parent/../escaped"
  local version="0.0.0-parent-signed-output"
  local rc=0
  mkdir -p "${case_dir}"
  write_manifest "${manifest}" "$(json_slot '"docs/attestations/schema.json"' "" "")"

  set +e
  PATH="${fake_bin}:$PATH" \
  COSIGN_IDENTITY="https://github.com/frankensuite/frankenterm/.github/workflows/release.yml@refs/tags/v${version}" \
  COSIGN_OIDC_ISSUER="https://token.actions.githubusercontent.com" \
  FT_ATTESTATION_MANIFEST="${manifest}" \
  FT_ATTESTATION_OUT_DIR="${parent_out_dir}" \
  FT_BEAD_ID="ft-e87u6.2" \
  FT_SCENARIO_ID="attestation_parent_signed_output_guard" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${version}" \
      --channel stable \
      --sign cosign >"${case_dir}/stdout.txt" 2>"${case_dir}/stderr.txt"
  rc=$?
  set -e

  total=$((total + 1))
  if [[ "${rc}" == "0" ]] || ! grep -q "repo-relative without parent traversal" "${case_dir}/stderr.txt"; then
    fail=$((fail + 1))
    record_result "${name}" "nonzero_parent_output_rejection" "${rc}" "failed" "${case_dir}"
    echo "FAIL ${name}: parent traversal signed output was not rejected" >&2
    return 0
  fi

  pass=$((pass + 1))
  record_result "${name}" "nonzero_parent_output_rejection" "${rc}" "passed" "${case_dir}"
  echo "PASS ${name}"
}

run_unsafe_retraction_path_index_case() {
  local name="unsafe_retraction_path_is_not_indexed"
  local case_dir="${ARTIFACT_ROOT}/${name}"
  local manifest="${case_dir}/manifest.json"
  local out_dir="${case_dir}/out"
  local retraction_root="${case_dir}/retractions-parent/../unsafe-retractions"
  local bundle_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local retraction_dir="${retraction_root}/${bundle_sha}"
  local version="0.0.0-unsafe-retraction-path"
  local rc=0
  mkdir -p "${case_dir}" "${out_dir}" "${retraction_dir}"
  write_manifest "${manifest}" "$(json_slot '"docs/attestations/schema.json"' "" "")"

  cat >"${retraction_dir}/perf__headline-claims.json" <<JSON
{
  "original_bundle_sha256": "${bundle_sha}",
  "affected_slot": "perf/headline-claims",
  "retracted_at": "2026-05-24T00:00:00Z",
  "retracted_by_release": "0.0.1-corrigendum",
  "retraction_rationale": "fixture path uses parent traversal and must not be indexed",
  "corrected_claim_value": null
}
JSON

  set +e
  FT_ATTESTATION_MANIFEST="${manifest}" \
  FT_ATTESTATION_OUT_DIR="${out_dir}" \
  FT_ATTESTATION_RETRACTIONS_ROOT="${retraction_root}" \
  FT_BEAD_ID="ft-e87u6.2" \
  FT_SCENARIO_ID="attestation_unsafe_retraction_path_guard" \
    bash "${ROOT_DIR}/scripts/attestation-build.sh" \
      --version "${version}" \
      --channel dev \
      --sign unsigned >"${case_dir}/stdout.txt" 2>"${case_dir}/stderr.txt"
  rc=$?
  set -e

  total=$((total + 1))
  if [[ "${rc}" != "0" ]]; then
    fail=$((fail + 1))
    record_result "${name}" "0" "${rc}" "failed" "${case_dir}"
    echo "FAIL ${name}: build rejected unsafe indexed retraction instead of skipping it" >&2
    return 0
  fi

  local bundle="${out_dir}/${version}.json"
  if ! jq -e '(.retractions // []) | length == 0' "${bundle}" >/dev/null \
    || ! grep -q "skipping retraction with unsafe repo-relative path" "${case_dir}/stderr.txt"; then
    fail=$((fail + 1))
    record_result "${name}" "skip_unsafe_retraction_path" "${rc}" "failed" "${case_dir}"
    echo "FAIL ${name}: unsafe retraction path was indexed or warning was missing" >&2
    return 0
  fi

  pass=$((pass + 1))
  record_result "${name}" "skip_unsafe_retraction_path" "${rc}" "passed" "${case_dir}"
  echo "PASS ${name}"
}

require_cmd bash
require_cmd jq
require_cmd git

run_case \
  "populated_slot_passes" \
  0 \
  '"docs/attestations/schema.json"' \
  "" \
  "" \
  0

run_case \
  "populated_slot_missing_file_fails" \
  1 \
  '"docs/attestations/does-not-exist-ft-e87u6-2.json"' \
  "" \
  "" \
  0

run_case \
  "absolute_artifact_path_fails" \
  1 \
  '"/tmp/ft-e87u6-2-outside.json"' \
  "" \
  "" \
  0

run_case \
  "parent_artifact_path_fails" \
  1 \
  '"../docs/attestations/schema.json"' \
  "" \
  "" \
  0

run_case \
  "dot_segment_artifact_path_fails" \
  1 \
  '"./docs/attestations/schema.json"' \
  "" \
  "" \
  0

run_case \
  "empty_segment_artifact_path_fails" \
  1 \
  '"docs//attestations/schema.json"' \
  "" \
  "" \
  0

run_case \
  "allow_partial_skips_missing_file" \
  0 \
  '"docs/attestations/does-not-exist-ft-e87u6-2.json"' \
  "" \
  "" \
  0 \
  --allow-partial

run_case \
  "deferred_with_live_bead_warns_only" \
  0 \
  'null' \
  "ft-e87u6.9" \
  "live recovery bead used by the behavior matrix" \
  1

run_case \
  "deferred_with_closed_bead_still_warns" \
  0 \
  'null' \
  "ft-i2eni.1" \
  "closed producer still remains visible to later drift tests" \
  1

run_case \
  "strict_deferred_blocks_release" \
  1 \
  'null' \
  "ft-e87u6.9" \
  "strict release mode rejects this deferred slot" \
  0 \
  --strict-deferred

run_case \
  "unfilled_slot_fails_default" \
  1 \
  'null' \
  "" \
  "" \
  0

run_case \
  "allow_partial_overrides_unfilled" \
  0 \
  'null' \
  "" \
  "" \
  0 \
  --allow-partial

run_unsafe_retraction_path_index_case
run_sigstore_cases
run_external_signed_output_case
run_parent_signed_output_case

jq -n \
  --arg bead_id "ft-e87u6.2" \
  --arg scenario_id "attestation_build_matrix" \
  --arg run_id "${RUN_ID}" \
  --arg artifact_root "${ARTIFACT_ROOT}" \
  --arg results_jsonl "${RESULTS_JSONL}" \
  --argjson total "${total}" \
  --argjson passed "${pass}" \
  --argjson failed "${fail}" \
  '{
    bead_id: $bead_id,
    scenario_id: $scenario_id,
    run_id: $run_id,
    artifact_root: $artifact_root,
    results_jsonl: $results_jsonl,
    counts: {total: $total, passed: $passed, failed: $failed},
    outcome: (if $failed == 0 then "passed" else "failed" end)
  }' > "${SUMMARY_FILE}"

if [[ "${fail}" -eq 0 ]]; then
  echo "PASS: attestation build matrix (${pass}/${total})"
  exit 0
fi

echo "FAIL: attestation build matrix (${fail} failed / ${total} total)" >&2
exit 1
