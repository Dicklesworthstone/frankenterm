#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_contract"
CORRELATION_ID="ft-1i2ge.1.6-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_contract_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mission_contract_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/mission_contract_${RUN_ID}.probe.log"

PROPTEST_CASES_VALUE="${PROPTEST_CASES:-100}"
PROPTEST_RNG_SEED_VALUE="${PROPTEST_RNG_SEED:-424242}"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "mission_contract.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "mission contract unit + property validation with structured logging"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq required for structured logging"
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "rch_required" \
    "rch_not_installed" \
    "$(basename "${LOG_FILE}")" \
    "rch is required for mission contract test execution"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -eq 0 ]] && grep -q "✓" "${PROBE_FILE}"; then
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_healthy" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running contract tests through healthy rch workers"
else
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_unavailable_fail_open" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running contract tests through rch fail-open path"
fi

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_json_roundtrip_preserves_required_fields -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_lifecycle_transition_conformance_matches_transition_table -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_failure_taxonomy_catalog_has_unique_reason_and_error_codes -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_contract_property_ -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="mission_contract_surface"
  reason_code="none"
  if [[ "${test_cmd}" == *"lifecycle_transition_conformance"* ]]; then
    decision_path="lifecycle_transition_matrix"
    reason_code="state_machine_conformance"
  elif [[ "${test_cmd}" == *"failure_taxonomy"* ]]; then
    decision_path="failure_taxonomy_matrix"
    reason_code="failure_code_catalog_validation"
  elif [[ "${test_cmd}" == *"mission_contract_property_"* ]]; then
    decision_path="property_invariant_path"
    reason_code="property_invariant_validation"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing with PROPTEST_CASES=${PROPTEST_CASES_VALUE} PROPTEST_RNG_SEED=${PROPTEST_RNG_SEED_VALUE}: rch exec -- env CARGO_TARGET_DIR=target-rch-mission-contract ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "rch exec -- env CARGO_TARGET_DIR=target-rch-mission-contract PROPTEST_CASES=${PROPTEST_CASES_VALUE} PROPTEST_RNG_SEED=${PROPTEST_RNG_SEED_VALUE} ${test_cmd}"
  ) 2>&1 | tee -a "${STDOUT_FILE}"
  status=${PIPESTATUS[0]}
  set -e

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" \
      "exit=${status}; command=${test_cmd}; seed=${PROPTEST_RNG_SEED_VALUE}; cases=${PROPTEST_CASES_VALUE}"
    exit ${status}
  fi
done

required_markers=(
  "mission_json_roundtrip_preserves_required_fields ... ok"
  "mission_lifecycle_transition_conformance_matches_transition_table ... ok"
  "mission_failure_taxonomy_catalog_has_unique_reason_and_error_codes ... ok"
  "mission_contract_property_transition_conformance_matches_table ... ok"
  "mission_contract_property_duplicate_candidate_ids_are_rejected ... ok"
  "mission_contract_property_blank_provenance_fields_are_rejected ... ok"
  "mission_contract_property_failure_code_roundtrips_are_stable ... ok"
  "mission_contract_property_terminal_states_require_matching_outcomes ... ok"
)

for marker in "${required_markers[@]}"; do
  if ! grep -q "${marker}" "${STDOUT_FILE}"; then
    emit_log \
      "failed" \
      "assertion_check" \
      "missing_success_marker" \
      "expected_test_marker_missing" \
      "$(basename "${STDOUT_FILE}")" \
      "Missing marker: ${marker}"
    exit 1
  fi
done

emit_log \
  "passed" \
  "schema_roundtrip->lifecycle_matrix->failure_taxonomy->property_invariants" \
  "mission_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission schema/state/failure invariants validated with deterministic property seeds and structured artifacts"
