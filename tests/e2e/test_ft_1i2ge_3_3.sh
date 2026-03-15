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
PROBE_FILE="${LOG_DIR}/ft_1i2ge_3_3_${RUN_ID}.probe.log"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
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

cmd_prefix="rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-3-3"
emit_log \
  "running" \
  "execution_preflight" \
  "rch_workers_healthy" \
  "none" \
  "$(basename "${PROBE_FILE}")" \
  "offloading tests through rch workers"

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_reconcile_assignment_signal_applies_success_and_completes -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reconcile_assignment_signal_timeout_maps_to_dispatch_error_failure -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reconcile_assignment_signal_ignores_out_of_order_update -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reconcile_assignment_signal_surfaces_drift_on_newer_conflict -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reconcile_assignment_signal_rejects_unknown_failure_code -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="nominal_path"
  reason_code="outcome_reconciliation_validation"

  if [[ "${test_cmd}" == *"timeout_maps_to_dispatch_error_failure"* ]]; then
    decision_path="failure_injection_path"
    reason_code="timeout_signal_failure_mapping"
  elif [[ "${test_cmd}" == *"ignores_out_of_order_update"* ]]; then
    decision_path="failure_injection_path"
    reason_code="late_signal_reconciliation"
  elif [[ "${test_cmd}" == *"surfaces_drift_on_newer_conflict"* ]]; then
    decision_path="recovery_path"
    reason_code="state_drift_detection"
  elif [[ "${test_cmd}" == *"rejects_unknown_failure_code"* ]]; then
    decision_path="failure_injection_path"
    reason_code="unknown_failure_code_rejected"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing: ${cmd_prefix} ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "${cmd_prefix} ${test_cmd}"
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
