#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.8.5 E2E: Commit-phase executor with deterministic ordering
#
# Validates:
# 1. All steps succeed → FullyCommitted
# 2. First step failure → ImmediateFailure
# 3. Partial failure trips barrier → PartialFailure
# 4. Kill-switch blocks all steps
# 5. Pause suspends execution
# 6. Missing step input treated as failure
# 7. Non-prepared state rejected
# 8. Committing state resume allowed
# 9. Empty plan rejected
# 10. Step outcome tag names
# 11. Outcome target tx states
# 12. Report canonical string determinism
# 13. Report serde roundtrip
# 14. Step result serde roundtrip
# 15. Step input serde roundtrip
# 16. Receipt seq monotonicity
# 17. Receipts continue from prior seq
# 18. Step results in ordinal order
# 19. Single step success
# 20. Single step failure
# 21. Outcome tag names
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_8_5_commit_phase"
CORRELATION_ID="ft-1i2ge.8.5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_8_5_${RUN_ID}.jsonl"
STDOUT_BASENAME="ft_1i2ge_8_5_${RUN_ID}"
REMOTE_SCRATCH_BASENAME="rch-e2e-ft-1i2ge-8-5-${RUN_ID}"
REMOTE_SCRATCH_ROOT="${RCH_REMOTE_SCRATCH_ROOT:-target/${REMOTE_SCRATCH_BASENAME}}"
REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-${REMOTE_SCRATCH_ROOT}/tmp}"
REMOTE_TARGET_DIR="${RCH_REMOTE_TARGET_DIR:-${REMOTE_SCRATCH_ROOT}/cargo-target}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_8_5"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

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
    --arg component "commit_phase.e2e" \
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
  "ft-1i2ge.8.5 commit-phase e2e"

emit_log \
  "started" \
  "remote_scratch_config" \
  "remote_scratch_paths_selected" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "remote_scratch_root=${REMOTE_SCRATCH_ROOT}; remote_tmpdir=${REMOTE_TMPDIR}; remote_target_dir=${REMOTE_TARGET_DIR}"

TESTS=(
  "commit_all_steps_succeed"
  "commit_first_step_fails_immediate_failure"
  "commit_partial_failure_trips_barrier"
  "commit_kill_switch_blocks_all_steps"
  "commit_kill_switch_hard_stop_blocks"
  "commit_paused_suspends_execution"
  "commit_missing_step_input_treated_as_failure"
  "commit_rejects_non_prepared_state"
  "commit_allows_committing_state_resume"
  "commit_rejects_empty_plan"
  "commit_step_outcome_tag_names"
  "commit_outcome_target_states"
  "commit_report_canonical_string_deterministic"
  "commit_report_serde_roundtrip"
  "commit_step_result_serde_roundtrip"
  "commit_step_input_serde_roundtrip"
  "commit_receipts_have_monotonic_seq"
  "commit_receipts_continue_from_prior"
  "commit_step_results_in_ordinal_order"
  "commit_single_step_success"
  "commit_single_step_failure"
  "commit_outcome_tag_names"
)

PASS_COUNT=0
FAIL_COUNT=0

for test_name in "${TESTS[@]}"; do
  test_stdout_file="${LOG_DIR}/${STDOUT_BASENAME}.${test_name}.stdout.log"

  emit_log "running" "cargo_test" "none" "none" \
    "$(basename "${test_stdout_file}")" "test=${test_name}"

  set +e
  run_rch_cargo_logged "${test_stdout_file}" \
    env TMPDIR="${REMOTE_TMPDIR}" CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    sh -c "mkdir -p \"\$TMPDIR\" \"\$CARGO_TARGET_DIR\" && exec cargo test -p frankenterm-core --lib \"\$1\" -- --nocapture" \
    sh "${test_name}"
  rc=$?
  set -e

  if [[ ${rc} -ne 0 ]]; then
    emit_log "failed" "cargo_test" "test_failure" "cargo_test_failed" \
      "$(basename "${test_stdout_file}")" "test=${test_name} exit=${rc}"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  else
    emit_log "passed" "cargo_test" "test_passed" "none" \
      "$(basename "${test_stdout_file}")" "test=${test_name}"
    PASS_COUNT=$((PASS_COUNT + 1))
  fi
done

if [[ ${FAIL_COUNT} -gt 0 ]]; then
  emit_log "failed" "suite_complete" "partial_failure" "tests_failed" \
    "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=${FAIL_COUNT}"
  echo "Commit-phase e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Commit-phase e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
