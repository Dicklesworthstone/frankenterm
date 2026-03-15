#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.8.10 E2E: Unit/property/concurrency correctness suite for tx semantics
#
# Validates:
# 1. State machine: commit requires prepared/committing
# 2. State machine: compensation requires compensating
# 3. Terminal/non-terminal state classification
# 4. Full commit → full rollback pipeline
# 5. Partial commit → partial rollback pipeline
# 6. First step failure → nothing to compensate
# 7. Commit step counts sum invariant
# 8. Compensation step counts sum invariant
# 9. Receipt monotonicity through commit
# 10. Receipt monotonicity through compensation
# 11. Receipt sequence continuation from prior
# 12. Idempotency lifecycle: fresh → commit → duplicate blocked
# 13. Resume after crash mid-commit
# 14. Step-level already_succeeded guard
# 15. Step key uniqueness across tx IDs
# 16. Resume no progress → all pending
# 17. Resume after full commit → no pending
# 18. Resume after partial commit → correct pending
# 19. Resume after full pipeline → resolved
# 20. Kill-switch blocks then idempotency allows retry
# 21. Pause suspends then idempotency allows retry
# 22. Deterministic commit replay
# 23. Deterministic compensation replay
# 24. Reason codes on success
# 25. Reason codes on failure
# 26. Serde roundtrip full pipeline
# 27. Commit step ordinal ordering
# 28. Compensation reverse ordinal ordering
# 29. Empty plan commit rejected
# 30. Commit outcome target states
# 31. Compensation outcome target states
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_8_10_tx_correctness"
CORRELATION_ID="ft-1i2ge.8.10-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-8-10-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_8_10_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_8_10_${RUN_ID}.stdout.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_8_10"
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
    --arg component "tx_correctness.e2e" \
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
  "ft-1i2ge.8.10 tx correctness e2e"

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

TESTS=(
  "sm_commit_requires_prepared_or_committing"
  "sm_commit_accepts_prepared"
  "sm_commit_accepts_committing"
  "sm_compensation_requires_compensating"
  "sm_terminal_states_are_terminal"
  "sm_non_terminal_states_are_not_terminal"
  "pipeline_full_commit_then_full_rollback"
  "pipeline_partial_commit_then_partial_rollback"
  "pipeline_first_step_failure_nothing_to_compensate"
  "pipeline_commit_step_counts_sum"
  "pipeline_compensation_step_counts_sum"
  "receipts_monotonic_through_commit"
  "receipts_monotonic_through_compensation"
  "receipts_continue_sequence_from_prior"
  "idempotency_full_lifecycle_fresh_commit_then_duplicate"
  "idempotency_resume_after_crash_mid_commit"
  "idempotency_step_level_already_succeeded_guard"
  "idempotency_step_key_uniqueness_across_tx_ids"
  "resume_no_progress_shows_all_pending"
  "resume_after_full_commit_no_pending"
  "resume_after_partial_commit_shows_correct_pending"
  "resume_after_full_pipeline_is_resolved"
  "killswitch_blocks_then_idempotency_allows_retry"
  "pause_suspends_then_idempotency_allows_retry"
  "deterministic_replay_same_inputs_same_results"
  "deterministic_compensation_replay"
  "reason_codes_on_success"
  "reason_codes_on_failure"
  "serde_roundtrip_full_pipeline"
  "commit_step_results_in_ordinal_order"
  "compensation_step_results_in_reverse_ordinal_order"
  "empty_plan_commit_rejected"
  "commit_outcome_target_states"
  "compensation_outcome_target_states"
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
      cargo test -p frankenterm-core --test tx_correctness_suite "${test_name}" -- --nocapture
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
  echo "TX Correctness e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "TX Correctness e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
