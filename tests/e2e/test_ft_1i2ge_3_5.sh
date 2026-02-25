#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.3.5 E2E: Pause/resume/abort semantics and checkpoint recovery
#
# Validates:
# 1. Pause from active states creates checkpoint with assignment snapshot
# 2. Resume restores prior lifecycle state from checkpoint
# 3. Abort cancels all in-flight assignments
# 4. Pause rejects terminal/invalid states
# 5. Resume rejects non-paused state
# 6. Abort from paused finalizes checkpoint + cancels
# 7. Cumulative pause duration tracking
# 8. Checkpoint history eviction bounds memory
# 9. Serde roundtrip preserves pause/resume state
# 10. Canonical string determinism
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_3_5_pause_resume_abort"
CORRELATION_ID="ft-1i2ge.3.5-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-3-5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_3_5_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_3_5_${RUN_ID}.stdout.log"

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
    --arg component "mission_pause_resume.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg decision_reason "${reason_code}" \
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
      decision_reason: $decision_reason,
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
  "ft-1i2ge.3.5 pause/resume/abort e2e"

# Preflight checks
if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" \
    "$(basename "${LOG_FILE}")" "jq is required"
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log "failed" "preflight_rch" "rch_missing" "rch_not_found" \
    "$(basename "${LOG_FILE}")" "rch must be installed"
  exit 1
fi

if ! rch workers probe --all --json \
  | jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    >/dev/null 2>&1; then
  emit_log "failed" "preflight_rch_workers" "rch_workers_unreachable" \
    "remote_worker_unavailable" "$(basename "${LOG_FILE}")" \
    "No reachable rch workers; aborting"
  exit 1
fi

# Run the unit tests for pause/resume/abort
TESTS=(
  "pause_from_running_creates_checkpoint"
  "pause_from_dispatching_creates_checkpoint"
  "pause_from_awaiting_approval_creates_checkpoint"
  "pause_from_blocked_creates_checkpoint"
  "pause_from_retry_pending_creates_checkpoint"
  "pause_rejects_terminal_states"
  "pause_rejects_already_paused"
  "pause_rejects_planning_state"
  "pause_rejects_empty_requested_by"
  "resume_restores_paused_from_state"
  "resume_rejects_not_paused"
  "resume_records_duration"
  "resume_cumulative_duration_tracking"
  "abort_from_running_cancels_all_assignments"
  "abort_from_paused_cancels"
  "abort_from_planning_cancels"
  "abort_rejects_terminal_states"
  "abort_rejects_empty_requested_by"
  "can_pause_guards_correct_states"
  "can_resume_only_when_paused"
  "can_abort_all_non_terminal"
  "checkpoint_captures_assignment_state"
  "checkpoint_history_bounded_by_eviction"
  "pause_resume_state_serde_roundtrip"
  "pause_resume_canonical_string_deterministic"
  "mission_control_command_serde_roundtrip"
)

PASS_COUNT=0
FAIL_COUNT=0

for test_name in "${TESTS[@]}"; do
  emit_log "running" "cargo_test" "none" "none" \
    "$(basename "${STDOUT_FILE}")" "test=${test_name}"

  set +e
  (
    cd "${ROOT_DIR}"
    env TMPDIR=/tmp rch exec -- \
      env CARGO_TARGET_DIR="${TARGET_DIR}" \
      cargo test -p frankenterm-core --lib "${test_name}" -- --nocapture
  ) >> "${STDOUT_FILE}" 2>&1
  rc=$?
  set -e

  if [[ ${rc} -ne 0 ]]; then
    emit_log "failed" "cargo_test" "test_failure" "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" "test=${test_name} exit=${rc}"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  else
    emit_log "passed" "cargo_test" "test_passed" "none" \
      "$(basename "${STDOUT_FILE}")" "test=${test_name}"
    PASS_COUNT=$((PASS_COUNT + 1))
  fi
done

if [[ ${FAIL_COUNT} -gt 0 ]]; then
  emit_log "failed" "suite_complete" "partial_failure" "tests_failed" \
    "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=${FAIL_COUNT}"
  echo "Pause/resume/abort e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Pause/resume/abort e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
