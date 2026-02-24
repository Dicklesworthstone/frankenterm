#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_1_2_mission_lifecycle"
CORRELATION_ID="ft-1i2ge.1.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_1_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_1_2_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_1_2_${RUN_ID}.probe.log"

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
    --arg component "mission_lifecycle.e2e" \
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
  "mission lifecycle state machine transition validation"

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch required for offloaded execution"
  exit 1
fi

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

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -ne 0 ]] || ! grep -q "✓" "${PROBE_FILE}"; then
  emit_log \
    "failed" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" \
    "No healthy remote RCH worker available"
  exit 1
fi

TEST_CMD=(
  rch exec --
  env CARGO_TARGET_DIR=target-rch-ft-1i2ge-1-2
  cargo test -p frankenterm-core --lib mission_ -- --nocapture
)

emit_log \
  "running" \
  "cargo_test" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Executing: ${TEST_CMD[*]}"

set +e
(
  cd "${ROOT_DIR}"
  "${TEST_CMD[@]}"
) 2>&1 | tee "${STDOUT_FILE}"
status=${PIPESTATUS[0]}
set -e

if grep -q "\[RCH\] local" "${STDOUT_FILE}"; then
  emit_log \
    "failed" \
    "cargo_test" \
    "rch_fallback_local" \
    "offload_contract_violation" \
    "$(basename "${STDOUT_FILE}")" \
    "rch fell back to local execution"
  exit 1
fi

if [[ ${status} -ne 0 ]]; then
  emit_log \
    "failed" \
    "cargo_test" \
    "test_failure" \
    "cargo_test_failed" \
    "$(basename "${STDOUT_FILE}")" \
    "exit=${status}"
  exit ${status}
fi

required_markers=(
  "mission_lifecycle_transition_table_contains_required_branches ... ok"
  "mission_lifecycle_happy_path_reaches_completed ... ok"
  "mission_lifecycle_retry_and_unblock_paths_are_supported ... ok"
  "mission_lifecycle_invalid_transition_is_rejected ... ok"
  "mission_validate_rejects_terminal_state_without_matching_outcome ... ok"
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
  "mission_lifecycle_transition_table->state_transition->validation_coherence" \
  "mission_lifecycle_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission lifecycle state machine validated with remote-only rch execution"
