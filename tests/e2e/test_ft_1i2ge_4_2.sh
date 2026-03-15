#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_4_2_reservation_enforcement"
CORRELATION_ID="ft-1i2ge.4.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_4_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_4_2_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_4_2_${RUN_ID}.probe.log"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_4_2"
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
    --arg component "mission_reservation_enforcement.e2e" \
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
  "mission reservation and ownership enforcement contract checks"

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

cmd_prefix="rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-4-2"
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
  "cargo test -p frankenterm-core --lib mission_policy_preflight_dispatch_time_requires_assignment_reference -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_policy_preflight_dispatch_time_accepts_assignment_bound_denial_and_feedback -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reservation_feasibility_denies_conflicting_lease_and_returns_feedback -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reservation_feasibility_allows_same_holder_or_expired_lease -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reservation_feasibility_marks_expired_intent_as_stale_state -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_reservation_paths_overlap_supports_wildcard_patterns -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="happy_path"
  reason_code="reservation_contract_validation"

  if [[ "${test_cmd}" == *"maps_wait_for_to_robot_wait_for"* ]]; then
    decision_path="edge_case_path"
    reason_code="non_reservation_action_path"
  elif [[ "${test_cmd}" == *"paths_overlap_supports_wildcard_patterns"* ]]; then
    decision_path="edge_case_path"
    reason_code="reservation_path_wildcard_matching"
  elif [[ "${test_cmd}" == *"requires_assignment_reference"* ]]; then
    decision_path="failure_injection_path"
    reason_code="dispatch_assignment_required"
  elif [[ "${test_cmd}" == *"accepts_assignment_bound_denial_and_feedback"* ]]; then
    decision_path="recovery_path"
    reason_code="reservation_conflict_feedback"
  elif [[ "${test_cmd}" == *"denies_conflicting_lease_and_returns_feedback"* ]]; then
    decision_path="failure_injection_path"
    reason_code="reservation_conflict_detected"
  elif [[ "${test_cmd}" == *"allows_same_holder_or_expired_lease"* ]]; then
    decision_path="recovery_path"
    reason_code="reservation_lifecycle_allows_progress"
  elif [[ "${test_cmd}" == *"marks_expired_intent_as_stale_state"* ]]; then
    decision_path="failure_injection_path"
    reason_code="reservation_intent_expired"
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
  "mission_dispatch_contract_maps_candidate_to_robot_and_coordination_primitives ... ok"
  "mission_dispatch_contract_maps_wait_for_to_robot_wait_for ... ok"
  "mission_policy_preflight_dispatch_time_requires_assignment_reference ... ok"
  "mission_policy_preflight_dispatch_time_accepts_assignment_bound_denial_and_feedback ... ok"
  "mission_reservation_feasibility_denies_conflicting_lease_and_returns_feedback ... ok"
  "mission_reservation_feasibility_allows_same_holder_or_expired_lease ... ok"
  "mission_reservation_feasibility_marks_expired_intent_as_stale_state ... ok"
  "mission_reservation_paths_overlap_supports_wildcard_patterns ... ok"
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
  "reservation_contract_mapping->dispatch_assignment_guard->reservation_conflict_feedback->lease_feasibility_and_ownership_enforcement" \
  "mission_reservation_enforcement_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission reservation/ownership enforcement checks validated with structured deny feedback"

echo "Mission reservation enforcement e2e passed. Logs: ${LOG_FILE_REL}"
