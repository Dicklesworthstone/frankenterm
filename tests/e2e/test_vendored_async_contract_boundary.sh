#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# E2E: Core↔Vendored Async Contract Boundary (ft-e34d9.10.5.4)
#
# Runs the full contract test suite (structural + behavioral) and produces
# a machine-parseable evidence bundle for audit/triage.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_4_async_contract"
CORRELATION_ID="ft-e34d9.10.5.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/vendored_async_contract_${RUN_ID}.jsonl"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts"
mkdir -p "${ARTIFACT_DIR}"

# Structured log emitter
emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="${5:-none}"
  local artifact_path="${6:-none}"
  local input_summary="${7:-}"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "vendored_async_contract.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

# ---- Phase 1: Structural / static analysis tests ----------------------------

echo "=== Phase 1: Structural contract verification ==="
emit_log "start" "structural" "phase1_start" "begin" "" "" "running vendored_async_contract_verification"

if cargo test -p frankenterm-core \
  --test vendored_async_contract_verification \
  --no-default-features \
  -- --test-threads=1 2>"${LOG_DIR}/structural_stderr_${RUN_ID}.txt" \
  | tee "${LOG_DIR}/structural_stdout_${RUN_ID}.txt" \
  | grep -E "^test result:" | tail -1 | grep -q "0 failed"; then

  STRUCTURAL_PASS=$(grep -c '... ok' "${LOG_DIR}/structural_stdout_${RUN_ID}.txt" || true)
  PASS_COUNT=$((PASS_COUNT + STRUCTURAL_PASS))
  emit_log "pass" "structural" "phase1_complete" "all_structural_tests_passed" "" \
    "${LOG_DIR}/structural_stdout_${RUN_ID}.txt" "passed=${STRUCTURAL_PASS}"
  echo "  PASS: ${STRUCTURAL_PASS} structural tests passed"
else
  STRUCTURAL_FAIL=$(grep -c 'FAILED' "${LOG_DIR}/structural_stdout_${RUN_ID}.txt" || true)
  FAIL_COUNT=$((FAIL_COUNT + STRUCTURAL_FAIL))
  emit_log "fail" "structural" "phase1_complete" "structural_tests_failed" \
    "test_failure" "${LOG_DIR}/structural_stdout_${RUN_ID}.txt" "failed=${STRUCTURAL_FAIL}"
  echo "  FAIL: structural tests failed (${STRUCTURAL_FAIL} failures)"
fi

# ---- Phase 2: Behavioral runtime tests --------------------------------------

echo "=== Phase 2: Behavioral contract verification ==="
emit_log "start" "behavioral" "phase2_start" "begin" "" "" "running vendored_async_contract_behavioral"

if cargo test -p frankenterm-core \
  --test vendored_async_contract_behavioral \
  --no-default-features \
  -- --test-threads=1 2>"${LOG_DIR}/behavioral_stderr_${RUN_ID}.txt" \
  | tee "${LOG_DIR}/behavioral_stdout_${RUN_ID}.txt" \
  | grep -E "^test result:" | tail -1 | grep -q "0 failed"; then

  BEHAVIORAL_PASS=$(grep -c '... ok' "${LOG_DIR}/behavioral_stdout_${RUN_ID}.txt" || true)
  PASS_COUNT=$((PASS_COUNT + BEHAVIORAL_PASS))
  emit_log "pass" "behavioral" "phase2_complete" "all_behavioral_tests_passed" "" \
    "${LOG_DIR}/behavioral_stdout_${RUN_ID}.txt" "passed=${BEHAVIORAL_PASS}"
  echo "  PASS: ${BEHAVIORAL_PASS} behavioral tests passed"
else
  BEHAVIORAL_FAIL=$(grep -c 'FAILED' "${LOG_DIR}/behavioral_stdout_${RUN_ID}.txt" || true)
  FAIL_COUNT=$((FAIL_COUNT + BEHAVIORAL_FAIL))
  emit_log "fail" "behavioral" "phase2_complete" "behavioral_tests_failed" \
    "test_failure" "${LOG_DIR}/behavioral_stdout_${RUN_ID}.txt" "failed=${BEHAVIORAL_FAIL}"
  echo "  FAIL: behavioral tests failed (${BEHAVIORAL_FAIL} failures)"
fi

# ---- Phase 3: Integration / compliance infrastructure -----------------------

echo "=== Phase 3: Contract integration tests ==="
emit_log "start" "integration" "phase3_start" "begin" "" "" "running vendored_async_contract_integration"

if cargo test -p frankenterm-core \
  --test vendored_async_contract_integration \
  --no-default-features \
  -- --test-threads=1 2>"${LOG_DIR}/integration_stderr_${RUN_ID}.txt" \
  | tee "${LOG_DIR}/integration_stdout_${RUN_ID}.txt" \
  | grep -E "^test result:" | tail -1 | grep -q "0 failed"; then

  INTEGRATION_PASS=$(grep -c '... ok' "${LOG_DIR}/integration_stdout_${RUN_ID}.txt" || true)
  PASS_COUNT=$((PASS_COUNT + INTEGRATION_PASS))
  emit_log "pass" "integration" "phase3_complete" "all_integration_tests_passed" "" \
    "${LOG_DIR}/integration_stdout_${RUN_ID}.txt" "passed=${INTEGRATION_PASS}"
  echo "  PASS: ${INTEGRATION_PASS} integration tests passed"
else
  INTEGRATION_FAIL=$(grep -c 'FAILED' "${LOG_DIR}/integration_stdout_${RUN_ID}.txt" || true)
  FAIL_COUNT=$((FAIL_COUNT + INTEGRATION_FAIL))
  emit_log "fail" "integration" "phase3_complete" "integration_tests_failed" \
    "test_failure" "${LOG_DIR}/integration_stdout_${RUN_ID}.txt" "failed=${INTEGRATION_FAIL}"
  echo "  FAIL: integration tests failed (${INTEGRATION_FAIL} failures)"
fi

# ---- Phase 4: Surface guard static analysis ---------------------------------

echo "=== Phase 4: Surface guard confinement tests ==="
emit_log "start" "surface_guard" "phase4_start" "begin" "" "" "running runtime_compat_surface_guard"

if cargo test -p frankenterm-core \
  --test runtime_compat_surface_guard \
  --no-default-features \
  -- --test-threads=1 2>"${LOG_DIR}/surface_guard_stderr_${RUN_ID}.txt" \
  | tee "${LOG_DIR}/surface_guard_stdout_${RUN_ID}.txt" \
  | grep -E "^test result:" | tail -1 | grep -q "0 failed"; then

  GUARD_PASS=$(grep -c '... ok' "${LOG_DIR}/surface_guard_stdout_${RUN_ID}.txt" || true)
  PASS_COUNT=$((PASS_COUNT + GUARD_PASS))
  emit_log "pass" "surface_guard" "phase4_complete" "all_guard_tests_passed" "" \
    "${LOG_DIR}/surface_guard_stdout_${RUN_ID}.txt" "passed=${GUARD_PASS}"
  echo "  PASS: ${GUARD_PASS} surface guard tests passed"
else
  GUARD_FAIL=$(grep -c 'FAILED' "${LOG_DIR}/surface_guard_stdout_${RUN_ID}.txt" || true)
  FAIL_COUNT=$((FAIL_COUNT + GUARD_FAIL))
  emit_log "fail" "surface_guard" "phase4_complete" "guard_tests_failed" \
    "test_failure" "${LOG_DIR}/surface_guard_stdout_${RUN_ID}.txt" "failed=${GUARD_FAIL}"
  echo "  FAIL: surface guard tests failed (${GUARD_FAIL} failures)"
fi

# ---- Phase 5: Repeat-run stability (determinism) ----------------------------

echo "=== Phase 5: Repeat-run stability (3 iterations) ==="
emit_log "start" "stability" "phase5_start" "begin" "" "" "3-pass determinism check"

STABILITY_OK=true
for iteration in 1 2 3; do
  if ! cargo test -p frankenterm-core \
    --test vendored_async_contract_behavioral \
    --no-default-features \
    -- --test-threads=1 \
    > "${LOG_DIR}/stability_run${iteration}_${RUN_ID}.txt" 2>&1; then
    STABILITY_OK=false
    emit_log "fail" "stability_run${iteration}" "phase5_iteration" \
      "stability_failure" "non_deterministic" \
      "${LOG_DIR}/stability_run${iteration}_${RUN_ID}.txt" "iteration=${iteration}"
    echo "  FAIL: stability run ${iteration} failed"
  fi
done

if [ "${STABILITY_OK}" = true ]; then
  emit_log "pass" "stability" "phase5_complete" "3_iterations_stable" "" "" "all_3_passed"
  echo "  PASS: all 3 stability runs passed"
else
  FAIL_COUNT=$((FAIL_COUNT + 1))
  emit_log "fail" "stability" "phase5_complete" "stability_failure" "non_deterministic"
  echo "  FAIL: repeat-run stability check failed"
fi

# ---- Summary ----------------------------------------------------------------

TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
echo ""
echo "=== Summary ==="
echo "  Total: ${TOTAL} | Pass: ${PASS_COUNT} | Fail: ${FAIL_COUNT} | Skip: ${SKIP_COUNT}"
echo "  Evidence log: ${LOG_FILE}"
echo "  Correlation ID: ${CORRELATION_ID}"

emit_log "$([ "${FAIL_COUNT}" -eq 0 ] && echo 'pass' || echo 'fail')" \
  "summary" "e2e_complete" \
  "total=${TOTAL},pass=${PASS_COUNT},fail=${FAIL_COUNT},skip=${SKIP_COUNT}" \
  "$([ "${FAIL_COUNT}" -eq 0 ] && echo 'none' || echo 'test_failure')" \
  "${LOG_FILE}" ""

if [ "${FAIL_COUNT}" -gt 0 ]; then
  echo "  VERDICT: FAIL"
  exit 1
fi

echo "  VERDICT: PASS"
exit 0
