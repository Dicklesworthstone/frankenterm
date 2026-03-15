#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.3.7 E2E: Orchestration integration and e2e scenario harness
#
# Validates:
# 1. Journal lifecycle transition append and replay
# 2. Journal checkpoint and recovery
# 3. Journal duplicate correlation rejected
# 4. Journal compaction preserves post-checkpoint
# 5. Journal kill-switch change entry
# 6. Journal recovery marker
# 7. Journal sync to mission state
# 8. Dedup state record and find
# 9. Dedup state evict before cutoff
# 10. Failure code terminality classification
# 11. Failure code retryability
# 12. Outcome canonical string deterministic
# 13. Outcome serde roundtrip
# 14. Kill-switch levels behavior
# 15. Kill-switch activation serde roundtrip
# 16. Mission canonical string deterministic
# 17. Mission canonical string includes journal state
# 18. Lifecycle terminal states
# 19. Lifecycle non-terminal states
# 20. Dispatch mechanism serde roundtrip
# 21. Dispatch idempotency key deterministic
# 22. Dispatch idempotency key differs by mechanism
# 23. Mission loop initial state
# 24. Mission loop trigger accumulates
# 25. Journal entry all kinds serde roundtrip
# 26. Journal state serde roundtrip
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_3_7_orchestration"
CORRELATION_ID="ft-1i2ge.3.7-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-3-7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_3_7_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_3_7_${RUN_ID}.stdout.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_3_7"
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
    --arg component "orchestration.e2e" \
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
  "ft-1i2ge.3.7 orchestration integration e2e"

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
  "journal_lifecycle_transition_append_and_replay"
  "journal_checkpoint_and_recovery"
  "journal_duplicate_correlation_rejected"
  "journal_compaction_preserves_post_checkpoint"
  "journal_kill_switch_change_entry"
  "journal_recovery_marker"
  "journal_sync_to_mission_state"
  "dedup_state_record_and_find"
  "dedup_state_evict_before_cutoff"
  "failure_code_terminality_classification"
  "failure_code_retryability"
  "outcome_canonical_string_deterministic"
  "outcome_serde_roundtrip"
  "kill_switch_levels_behavior"
  "kill_switch_activation_serde_roundtrip"
  "mission_canonical_string_deterministic"
  "mission_canonical_string_includes_journal_state"
  "lifecycle_terminal_states"
  "lifecycle_non_terminal_states"
  "dispatch_mechanism_serde_roundtrip"
  "dispatch_idempotency_key_deterministic"
  "dispatch_idempotency_key_differs_by_mechanism"
  "mission_loop_initial_state"
  "mission_loop_trigger_accumulates"
  "journal_entry_all_kinds_serde_roundtrip"
  "journal_state_serde_roundtrip"
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
      cargo test -p frankenterm-core --test orchestration_integration "${test_name}" -- --nocapture
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
  echo "Orchestration integration e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Orchestration integration e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
