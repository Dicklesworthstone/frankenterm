#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_1_4_validators_conformance"
CORRELATION_ID="ft-1i2ge.1.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_1_4_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_1_4_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_1_4_${RUN_ID}.probe.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_1_4"
ensure_rch_ready

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
    --arg component "mission_contract_validators.e2e" \
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
  "mission snapshot validators + transition conformance"

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
    "rch is required for cargo test execution"
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
    "running validator tests through healthy rch workers"
else
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_unavailable_fail_open" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running validator tests through rch fail-open path"
fi

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_lifecycle_transition_conformance_matches_transition_table -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_lifecycle_happy_path_reaches_completed -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_lifecycle_invalid_transition_is_rejected -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_validate_rejects_empty_candidate_id_with_field_path -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_validate_rejects_non_monotonic_assignment_timestamps -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_validate_rejects_empty_reservation_path_entry -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="schema_and_transition_contract"
  reason_code="none"
  if [[ "${test_cmd}" == *"invalid_transition"* ]]; then
    decision_path="failure_injection_path"
    reason_code="invalid_transition_rejected"
  elif [[ "${test_cmd}" == *"happy_path_reaches_completed"* ]]; then
    decision_path="recovery_path"
    reason_code="valid_transition_sequence"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing: rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-1-4 ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-1-4 ${test_cmd}"
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
      "exit=${status}; command=${test_cmd}"
    exit ${status}
  fi
done

required_markers=(
  "mission_lifecycle_transition_conformance_matches_transition_table ... ok"
  "mission_lifecycle_happy_path_reaches_completed ... ok"
  "mission_lifecycle_invalid_transition_is_rejected ... ok"
  "mission_validate_rejects_empty_candidate_id_with_field_path ... ok"
  "mission_validate_rejects_non_monotonic_assignment_timestamps ... ok"
  "mission_validate_rejects_empty_reservation_path_entry ... ok"
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
  "snapshot_validation->transition_conformance->failure_injection->recovery" \
  "validator_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission validators/conformance contract validated with deterministic structured artifacts"
