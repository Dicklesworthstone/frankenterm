#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_5_2_robot_mission_endpoints"
CORRELATION_ID="ft-1i2ge.5.2-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-5-2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_5_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_5_2_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_5_2_${RUN_ID}.probe.log"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"

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
    --arg component "mission_robot_endpoints.e2e" \
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
  "robot mission state/decisions contract validation"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "rch_required_missing" \
    "rch_not_installed" \
    "$(basename "${LOG_FILE}")" \
    "rch is required for cargo execution in this scenario"
  echo "rch is required for this e2e scenario; refusing local cargo execution." >&2
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -ne 0 ]] || grep -q "✗" "${PROBE_FILE}"; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "rch_workers_unhealthy" \
    "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" \
    "rch workers probe failed; refusing local fallback"
  echo "rch workers are unavailable; refusing local cargo execution." >&2
  exit 1
fi

emit_log \
  "running" \
  "execution_preflight" \
  "rch_workers_healthy" \
  "none" \
  "$(basename "${PROBE_FILE}")" \
  "offloading tests through rch workers"

TESTS=(
  "mission_robot_command_family_parses_all_subcommands"
  "mission_robot_filters_validate_edge_inputs"
  "mission_robot_state_returns_empty_when_mission_filter_mismatches"
  "mission_robot_filters_apply_state_and_assignment_constraints"
  "mission_robot_decisions_include_explainability_payloads"
  "robot_mission_error_code_mapping_is_stable"
)

: >"${STDOUT_FILE}"
for test_name in "${TESTS[@]}"; do
  decision_path="state_contract"
  reason_code="robot_mission_contract_validation"
  if [[ "${test_name}" == *"validate_edge_inputs"* ]] || [[ "${test_name}" == *"mismatches"* ]]; then
    decision_path="failure_injection_path"
    reason_code="invalid_filter_and_state_mismatch"
  elif [[ "${test_name}" == *"decisions_include_explainability"* ]]; then
    decision_path="recovery_path"
    reason_code="explainability_payload_recovery"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing: cargo test -p frankenterm --bin ft ${test_name} -- --nocapture"

  set +e
  (
    cd "${ROOT_DIR}"
    env TMPDIR=/tmp rch exec -- \
      env CARGO_TARGET_DIR="${TARGET_DIR}" \
      cargo test -p frankenterm --bin ft "${test_name}" -- --nocapture
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
      "exit=${status}; test=${test_name}"
    exit "${status}"
  fi
done

for test_name in "${TESTS[@]}"; do
  if ! grep -q "${test_name} .* ok" "${STDOUT_FILE}"; then
    emit_log \
      "failed" \
      "assertion_check" \
      "missing_success_marker" \
      "expected_test_marker_missing" \
      "$(basename "${STDOUT_FILE}")" \
      "Missing success marker for ${test_name}"
    exit 1
  fi
done

emit_log \
  "passed" \
  "state->failure_injection->recovery->decisions" \
  "robot_mission_surface_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Robot mission state/decision endpoints validated with deterministic filter and explainability contracts"

echo "Robot mission endpoint e2e passed. Logs: ${LOG_FILE_REL}"
