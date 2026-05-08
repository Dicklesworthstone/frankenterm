#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_3_3_outcome_ingestion_reconciliation"
CORRELATION_ID="ft-1i2ge.3.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_3_3_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_3_3_${RUN_ID}.stdout.log"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

GUARD_LIB="$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_3_3"
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
    --arg component "mission_outcome_reconciliation.e2e" \
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

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "mission outcome-ingestion + assignment reconciliation contract checks"

DEFAULT_RCH_TARGET_DIR="target/rch-e2e-ft-1i2ge-3-3-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${REQUESTED_RCH_TARGET_DIR}" && "${REQUESTED_RCH_TARGET_DIR}" != /* ]]; then
  RCH_TARGET_DIR="${REQUESTED_RCH_TARGET_DIR}"
else
  RCH_TARGET_DIR="${DEFAULT_RCH_TARGET_DIR}"
fi
TEST_FILTERS=(
  "mission_reconcile_assignment_signal_applies_success_and_completes"
  "mission_reconcile_assignment_signal_timeout_maps_to_dispatch_error_failure"
  "mission_reconcile_assignment_signal_ignores_out_of_order_update"
  "mission_reconcile_assignment_signal_surfaces_drift_on_newer_conflict"
  "mission_reconcile_assignment_signal_rejects_unknown_failure_code"
)

: >"${STDOUT_FILE}"
command_index=0
for test_filter in "${TEST_FILTERS[@]}"; do
  command_index=$((command_index + 1))
  step_stdout="${LOG_DIR}/ft_1i2ge_3_3_${RUN_ID}.step_${command_index}.stdout.log"
  test_cmd=(env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib "${test_filter}" -- --nocapture)
  decision_path="nominal_path"
  reason_code="outcome_reconciliation_validation"

  if [[ "${test_filter}" == *"timeout_maps_to_dispatch_error_failure"* ]]; then
    decision_path="failure_injection_path"
    reason_code="timeout_signal_failure_mapping"
  elif [[ "${test_filter}" == *"ignores_out_of_order_update"* ]]; then
    decision_path="failure_injection_path"
    reason_code="late_signal_reconciliation"
  elif [[ "${test_filter}" == *"surfaces_drift_on_newer_conflict"* ]]; then
    decision_path="recovery_path"
    reason_code="state_drift_detection"
  elif [[ "${test_filter}" == *"rejects_unknown_failure_code"* ]]; then
    decision_path="failure_injection_path"
    reason_code="unknown_failure_code_rejected"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing through rch: ${test_cmd[*]}"

  set +e
  run_rch_cargo_logged "${step_stdout}" "${test_cmd[@]}"
  status=$?
  set -e
  cat "${step_stdout}" >>"${STDOUT_FILE}"

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" \
      "exit=${status}; command=${test_cmd[*]}"
    exit "${status}"
  fi
done

required_markers=(
  "mission_reconcile_assignment_signal_applies_success_and_completes ... ok"
  "mission_reconcile_assignment_signal_timeout_maps_to_dispatch_error_failure ... ok"
  "mission_reconcile_assignment_signal_ignores_out_of_order_update ... ok"
  "mission_reconcile_assignment_signal_surfaces_drift_on_newer_conflict ... ok"
  "mission_reconcile_assignment_signal_rejects_unknown_failure_code ... ok"
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
  "signal_ingest->outcome_normalization->late_event_handling->drift_detection->lifecycle_reconciliation" \
  "mission_outcome_reconciliation_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission assignment outcome ingestion and reconciliation validated"

echo "Mission outcome reconciliation e2e passed. Logs: ${LOG_FILE_REL}"
