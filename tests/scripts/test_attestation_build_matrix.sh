#!/usr/bin/env bash
# ft-e87u6.2 -- attestation-build deferred-slot behavior matrix.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_ROOT="${FT_E87U6_2_ARTIFACT_DIR:-${TMPDIR:-/tmp}/ft-e87u6-2-attestation-build-matrix-${RUN_ID}}"
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
