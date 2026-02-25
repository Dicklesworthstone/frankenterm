#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.3.8 E2E: Crash-consistent mission journal and deterministic restart
#
# Validates:
# 1. Journal append increments sequence numbers monotonically
# 2. Duplicate correlation IDs are rejected (idempotency guard)
# 3. Checkpoint records mission state snapshot
# 4. Recovery marker appends correctly
# 5. Entries_since returns correct subset
# 6. Compact removes only entries below threshold
# 7. Compact preserves correlation index consistency
# 8. Needs_compaction respects configured limit
# 9. Journal snapshot_state captures metadata
# 10. Snapshot is clean after checkpoint
# 11. Replay from checkpoint reports correct counts
# 12. Replay detects sequence regression
# 13. Mission create_journal uses mission_id
# 14. Mission sync_journal_state propagates metadata
# 15. Journal lifecycle transition helper
# 16. Journal kill-switch change helper
# 17. Journal assignment outcome helper
# 18. Journal control command helper
# 19. Multiple checkpoints track the latest
# 20. Compact preserves post-checkpoint entries
# 21. Entry kind tag names
# 22. Journal state serde roundtrip
# 23. Journal entry serde roundtrip
# 24. Entry canonical string determinism
# 25. All entry kinds serde roundtrip
# 26. Control command entry serde roundtrip
# 27. Replay report serde roundtrip
# 28. Mission canonical_string includes journal state
# 29. Journal error Display messages
# 30. Replay report total_entries and is_clean
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_3_8_journal_recovery"
CORRELATION_ID="ft-1i2ge.3.8-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-3-8-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_3_8_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_3_8_${RUN_ID}.stdout.log"

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
    --arg component "mission_journal.e2e" \
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
  "ft-1i2ge.3.8 journal/recovery e2e"

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

# Unit tests
TESTS=(
  "journal_new_is_empty"
  "journal_append_increments_seq"
  "journal_duplicate_correlation_rejected"
  "journal_has_correlation"
  "journal_checkpoint_records_mission_state"
  "journal_recovery_marker"
  "journal_entries_since_returns_subset"
  "journal_compact_before_removes_entries"
  "journal_needs_compaction_respects_limit"
  "journal_snapshot_state_captures_metadata"
  "journal_snapshot_clean_after_checkpoint"
  "journal_replay_from_checkpoint_reports_counts"
  "journal_replay_detects_seq_regression"
  "journal_entry_kind_tag_names"
  "journal_state_serde_roundtrip"
  "journal_state_canonical_string_deterministic"
  "journal_entry_serde_roundtrip"
  "journal_entry_canonical_string_deterministic"
  "mission_create_journal_uses_mission_id"
  "mission_sync_journal_state"
  "journal_lifecycle_transition_helper"
  "journal_kill_switch_change_helper"
  "journal_assignment_outcome_helper"
  "journal_control_command_helper"
  "journal_multiple_checkpoints_track_last"
  "journal_compact_preserves_post_checkpoint_entries"
  "journal_error_display"
  "journal_replay_report_total_entries"
  "journal_replay_report_with_errors"
  "journal_mission_canonical_string_includes_journal"
  "journal_entry_all_kinds_serde_roundtrip"
  "journal_control_command_entry_serde_roundtrip"
  "journal_replay_report_serde_roundtrip"
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
  echo "Journal/recovery e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Journal/recovery e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
