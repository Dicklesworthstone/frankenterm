#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_3_2_dispatch_adapter"
CORRELATION_ID="ft-1i2ge.3.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_3_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_3_2_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_3_2_${RUN_ID}.probe.log"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_3_2"
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
    --arg component "mission_dispatch_adapter.e2e" \
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
  "mission dispatch adapter contract validation (target resolution + dry-run/live normalization)"

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

cmd_prefix="rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-3-2"
emit_log \
  "running" \
  "execution_preflight" \
  "rch_workers_healthy" \
  "none" \
  "$(basename "${PROBE_FILE}")" \
  "offloading tests through rch workers"

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_dispatch_contract_maps_candidate_to_robot_and_coordination_primitives -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_contract_maps_wait_for_to_robot_wait_for -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_resolves_target_with_pane_agent_and_thread -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_wait_for_target_resolves_pane_from_condition -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_rejects_unknown_assignment -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_dry_run_normalizes_success_outcome -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_live_success_defaults_reason_code -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_live_failure_normalizes_reason_and_error_code -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_live_failure_rejects_unknown_reason_code -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_dispatch_adapter_live_failure_rejects_mismatched_error_code -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="contract_surface"
  reason_code="none"
  if [[ "${test_cmd}" == *"rejects_"* ]]; then
    decision_path="failure_injection_path"
    reason_code="dispatch_failure_contract_checks"
  elif [[ "${test_cmd}" == *"dry_run"* ]]; then
    decision_path="dry_run_path"
    reason_code="dry_run_normalization"
  elif [[ "${test_cmd}" == *"live_success"* ]] || [[ "${test_cmd}" == *"normalizes_reason_and_error_code"* ]]; then
    decision_path="live_dispatch_path"
    reason_code="live_outcome_normalization"
  elif [[ "${test_cmd}" == *"wait_for"* ]] || [[ "${test_cmd}" == *"resolves_target"* ]]; then
    decision_path="target_resolution_path"
    reason_code="target_resolution_validation"
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
    exit ${status}
  fi
done

required_markers=(
  "mission_dispatch_contract_maps_candidate_to_robot_and_coordination_primitives ... ok"
  "mission_dispatch_contract_maps_wait_for_to_robot_wait_for ... ok"
  "mission_dispatch_adapter_resolves_target_with_pane_agent_and_thread ... ok"
  "mission_dispatch_adapter_wait_for_target_resolves_pane_from_condition ... ok"
  "mission_dispatch_adapter_rejects_unknown_assignment ... ok"
  "mission_dispatch_adapter_dry_run_normalizes_success_outcome ... ok"
  "mission_dispatch_adapter_live_success_defaults_reason_code ... ok"
  "mission_dispatch_adapter_live_failure_normalizes_reason_and_error_code ... ok"
  "mission_dispatch_adapter_live_failure_rejects_unknown_reason_code ... ok"
  "mission_dispatch_adapter_live_failure_rejects_mismatched_error_code ... ok"
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
  "assignment_target_resolution->dry_run_dispatch->live_dispatch_normalization->mission_outcome_contract" \
  "dispatch_adapter_pipeline_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission dispatch adapter validated with deterministic target resolution and outcome normalization contracts"

echo "Mission dispatch adapter e2e passed. Logs: ${LOG_FILE_REL}"
