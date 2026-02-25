#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.8.7 E2E: Durable idempotency, dedupe, and resume invariants
#
# Validates:
# 1. Fresh verdict when no prior record
# 2. Exact duplicate detection on terminal same-key
# 3. Double-commit blocking (FTX3001)
# 4. Double-compensation blocking (FTX3002)
# 5. Conflicting prior detection (FTX3003)
# 6. Resumable verdict on non-terminal
# 7. Tx key determinism
# 8. Tx key differs for different contracts
# 9. Step key determinism
# 10. Step key phase-sensitivity
# 11. TxPhase tag names
# 12. Execution record terminal detection
# 13. Execution record serde roundtrip
# 14. Step execution record serde roundtrip
# 15. Idempotency check result serde roundtrip
# 16. Idempotency verdict tag names
# 17. Resume state from empty contract
# 18. Resume state with full commit
# 19. Resume state with partial commit
# 20. Resume state terminal detection
# 21. Resume state canonical string determinism
# 22. All canonical strings deterministic
# 23. Step record phase-aware already_succeeded
# 24. Resume state serde roundtrip
# 25. TxPhase serde roundtrip
# 26. Verdict serde roundtrip
# 27. Resume state has_pending_work
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_8_7_idempotency_resume"
CORRELATION_ID="ft-1i2ge.8.7-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-8-7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_8_7_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_8_7_${RUN_ID}.stdout.log"

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
    --arg component "idempotency_resume.e2e" \
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
  "ft-1i2ge.8.7 idempotency/resume e2e"

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
  "idempotency_fresh_when_no_prior_record"
  "idempotency_exact_duplicate_on_terminal_same_key"
  "idempotency_double_commit_blocked"
  "idempotency_double_compensation_blocked"
  "idempotency_conflicting_prior_different_key"
  "idempotency_resumable_on_non_terminal"
  "idempotency_tx_key_deterministic"
  "idempotency_tx_key_differs_for_different_contracts"
  "step_key_deterministic"
  "step_key_differs_by_phase"
  "tx_phase_tag_names"
  "execution_record_is_terminal"
  "execution_record_serde_roundtrip"
  "step_execution_record_serde_roundtrip"
  "idempotency_check_result_serde_roundtrip"
  "idempotency_verdict_tag_names"
  "resume_state_from_empty_contract"
  "resume_state_with_full_commit"
  "resume_state_with_partial_commit"
  "resume_state_is_fully_resolved_on_terminal"
  "resume_state_canonical_string_deterministic"
  "execution_record_canonical_string_deterministic"
  "step_execution_record_canonical_string_deterministic"
  "idempotency_check_canonical_string_deterministic"
  "step_record_already_succeeded_checks_phase"
  "resume_state_serde_roundtrip"
  "tx_phase_serde_roundtrip"
  "idempotency_verdict_serde_roundtrip"
  "resume_state_has_pending_work"
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
  echo "Idempotency/Resume e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Idempotency/Resume e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
