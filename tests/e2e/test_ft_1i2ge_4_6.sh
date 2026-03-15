#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-1i2ge.4.6 E2E: Global kill-switch and safe-mode degradation
#
# Validates:
# 1. Kill-switch level semantics (Off, SafeMode, HardStop)
# 2. Activate/deactivate lifecycle with audit trail
# 3. TTL-based auto-expiry
# 4. In-flight cancellation under HardStop
# 5. MissionFailureCode::KillSwitchActivated taxonomy
# 6. Serde roundtrip preserves kill-switch state
# 7. Canonical string determinism and mission hash integration
# 8. Validation rejects invalid inputs (empty fields, bad TTL)
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_4_6_kill_switch"
CORRELATION_ID="ft-1i2ge.4.6-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-4-6-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_4_6_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_4_6_${RUN_ID}.stdout.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_4_6"
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
    --arg component "mission_kill_switch.e2e" \
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
  "ft-1i2ge.4.6 kill-switch e2e"

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

# Run the unit tests for kill-switch
TESTS=(
  "mission_kill_switch_level_defaults_to_off"
  "mission_kill_switch_level_blocks_dispatch"
  "mission_kill_switch_level_cancels_in_flight"
  "mission_kill_switch_level_allows_read_only"
  "mission_kill_switch_activate_records_and_sets_level"
  "mission_kill_switch_deactivate_returns_to_off"
  "mission_kill_switch_ttl_expiry_auto_deactivates"
  "mission_kill_switch_escalation_overwrites_level"
  "mission_kill_switch_evict_history_bounds_memory"
  "mission_evaluate_kill_switch_off_allows_dispatch"
  "mission_activate_kill_switch_blocks_dispatch"
  "mission_activate_kill_switch_rejects_level_off"
  "mission_activate_kill_switch_rejects_empty_activated_by"
  "mission_activate_kill_switch_rejects_expired_before_activated"
  "mission_deactivate_kill_switch_restores_dispatch"
  "mission_cancel_in_flight_for_kill_switch"
  "mission_kill_switch_serde_roundtrip"
  "mission_kill_switch_canonical_string_is_deterministic"
  "mission_kill_switch_failure_code_roundtrip"
  "mission_kill_switch_canonical_string_included_in_mission_hash"
  "mission_kill_switch_activation_expiry_check"
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
  echo "Kill-switch e2e FAILED: ${PASS_COUNT} passed, ${FAIL_COUNT} failed. Logs: ${LOG_FILE}"
  exit 1
fi

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" "passed=${PASS_COUNT} failed=0"

echo "Kill-switch e2e passed (${PASS_COUNT} tests). Logs: ${LOG_FILE}"
