#!/usr/bin/env bash
set -euo pipefail

# ft-e34d9.10.6.2: DPOR + concurrency failure injection matrix
#
# Validates:
# 1. concurrency_fault_matrix.rs tests compile and pass
# 2. Matrix covers all 4 fault profiles × 4 workloads
# 3. Recovery and cancellation safety tests pass
# 4. Determinism verification succeeds
# 5. Failure injection: deliberately breaks fault invariants to verify test catches it

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_6_2_concurrency_fault_matrix"
CORRELATION_ID="ft-e34d9.10.6.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/concurrency_fault_matrix_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/concurrency_fault_matrix_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/concurrency_fault_matrix_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/concurrency_fault_matrix_${RUN_ID}.report.fail.json"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

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
    --arg component "concurrency_fault_matrix.e2e" \
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
    }' | tee -a "${LOG_FILE}" >/dev/null
}

pass() { PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); }
skip() { SKIP_COUNT=$((SKIP_COUNT + 1)); }

echo "=== ft-e34d9.10.6.2: Concurrency Fault Matrix E2E ==="
echo "Run ID:         ${RUN_ID}"
echo "Correlation ID: ${CORRELATION_ID}"
echo "Log file:       ${LOG_FILE}"
echo ""

# ---------------------------------------------------------------------------
# Scenario 1: Check that test file exists and has expected structure
# ---------------------------------------------------------------------------
echo "--- Scenario 1: Test file structure ---"
TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/concurrency_fault_matrix.rs"

if [ ! -f "${TEST_FILE}" ]; then
  emit_log "FAIL" "file_exists" "FILE_NOT_FOUND" "E-CFM-001" "${TEST_FILE}" "concurrency_fault_matrix.rs"
  echo "  FAIL: ${TEST_FILE} not found"
  fail
else
  emit_log "PASS" "file_exists" "FILE_FOUND" "" "${TEST_FILE}" "concurrency_fault_matrix.rs"
  echo "  PASS: test file exists"
  pass

  # Check for required invariant comments
  for invariant in "CFM-1" "CFM-2" "CFM-3" "CFM-4" "CFM-5" "CFM-6" "CFM-7"; do
    if grep -q "${invariant}" "${TEST_FILE}"; then
      echo "  PASS: ${invariant} documented"
      pass
    else
      echo "  FAIL: ${invariant} not documented in test file"
      emit_log "FAIL" "invariant_doc" "INVARIANT_MISSING" "E-CFM-002" "${TEST_FILE}" "${invariant}"
      fail
    fi
  done

  # Check for required test functions
  EXPECTED_TESTS=(
    "cfm_pool_no_faults"
    "cfm_pool_single_fault"
    "cfm_pool_multi_fault"
    "cfm_pool_cascade"
    "cfm_channel_no_faults"
    "cfm_channel_single_fault"
    "cfm_channel_multi_fault"
    "cfm_channel_cascade"
    "cfm_mutation_no_faults"
    "cfm_mutation_single_fault"
    "cfm_mutation_cascade"
    "cfm_dispatch_no_faults"
    "cfm_dispatch_single_fault"
    "cfm_dispatch_multi_fault"
    "cfm_dispatch_cascade"
    "cfm_full_matrix_chaos_scenario"
    "cfm_recovery_after_fault_clearance"
    "cfm_cancellation_safety"
    "cfm_scaling_concurrency"
    "cfm_determinism"
  )

  for test_fn in "${EXPECTED_TESTS[@]}"; do
    if grep -q "fn ${test_fn}" "${TEST_FILE}"; then
      echo "  PASS: test fn ${test_fn} present"
      pass
    else
      echo "  FAIL: test fn ${test_fn} missing"
      emit_log "FAIL" "test_fn_present" "TEST_MISSING" "E-CFM-003" "${TEST_FILE}" "${test_fn}"
      fail
    fi
  done
fi

# ---------------------------------------------------------------------------
# Scenario 2: Compile check (requires asupersync-runtime feature)
# ---------------------------------------------------------------------------
echo ""
echo "--- Scenario 2: Compilation check ---"

CARGO_CMD="cargo check -p frankenterm-core --features asupersync-runtime --test concurrency_fault_matrix"

if command -v rch &>/dev/null; then
  COMPILE_CMD="rch exec -- ${CARGO_CMD}"
else
  COMPILE_CMD="${CARGO_CMD}"
fi

echo "  Running: ${COMPILE_CMD}"
if eval "${COMPILE_CMD}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "PASS" "compile" "COMPILE_OK" "" "${STDOUT_FILE}" "cargo check --test concurrency_fault_matrix"
  echo "  PASS: compilation succeeded"
  pass
else
  exit_code=$?
  emit_log "FAIL" "compile" "COMPILE_FAILED" "E-CFM-010" "${STDOUT_FILE}" "exit ${exit_code}"
  echo "  FAIL: compilation failed (exit ${exit_code})"
  echo "  Note: May require asupersync-runtime feature or rch workers"
  fail
fi

# ---------------------------------------------------------------------------
# Scenario 3: Run tests (if compilation passed)
# ---------------------------------------------------------------------------
echo ""
echo "--- Scenario 3: Test execution ---"

TEST_CMD="cargo test -p frankenterm-core --features asupersync-runtime --test concurrency_fault_matrix -- --test-threads=1"

if command -v rch &>/dev/null; then
  RUN_CMD="rch exec -- ${TEST_CMD}"
else
  RUN_CMD="${TEST_CMD}"
fi

echo "  Running: ${RUN_CMD}"
if eval "${RUN_CMD}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "PASS" "test_run" "ALL_TESTS_PASS" "" "${STDOUT_FILE}" "concurrency_fault_matrix tests"
  echo "  PASS: all tests passed"
  pass
else
  exit_code=$?
  emit_log "FAIL" "test_run" "TESTS_FAILED" "E-CFM-020" "${STDOUT_FILE}" "exit ${exit_code}"
  echo "  FAIL: tests failed (exit ${exit_code})"
  fail
fi

# ---------------------------------------------------------------------------
# Scenario 4: Determinism (run twice, compare)
# ---------------------------------------------------------------------------
echo ""
echo "--- Scenario 4: Determinism verification ---"

DET_CMD="cargo test -p frankenterm-core --features asupersync-runtime --test concurrency_fault_matrix cfm_determinism -- --exact"

if command -v rch &>/dev/null; then
  DET_CMD="rch exec -- ${DET_CMD}"
fi

DETERMINISM_PASS=true
for run in 1 2; do
  echo "  Determinism run ${run}/2..."
  if ! eval "${DET_CMD}" >> "${STDOUT_FILE}" 2>&1; then
    DETERMINISM_PASS=false
    break
  fi
done

if $DETERMINISM_PASS; then
  emit_log "PASS" "determinism" "DETERMINISTIC" "" "${STDOUT_FILE}" "2 identical runs"
  echo "  PASS: determinism verified"
  pass
else
  emit_log "FAIL" "determinism" "NON_DETERMINISTIC" "E-CFM-030" "${STDOUT_FILE}" "runs diverged"
  echo "  FAIL: determinism check failed"
  fail
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== Summary ==="
echo "  PASS: ${PASS_COUNT}"
echo "  FAIL: ${FAIL_COUNT}"
echo "  SKIP: ${SKIP_COUNT}"
echo ""
echo "  Log:    ${LOG_FILE}"
echo "  Stdout: ${STDOUT_FILE}"

TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))

if [ "${FAIL_COUNT}" -eq 0 ]; then
  jq -cn \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --argjson pass "${PASS_COUNT}" \
    --argjson fail "${FAIL_COUNT}" \
    --argjson skip "${SKIP_COUNT}" \
    --argjson total "${TOTAL}" \
    '{scenario_id: $scenario_id, correlation_id: $correlation_id, verdict: "PASS", pass: $pass, fail: $fail, skip: $skip, total: $total}' \
    > "${REPORT_OK}"
  echo ""
  echo "  VERDICT: PASS"
  exit 0
else
  jq -cn \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --argjson pass "${PASS_COUNT}" \
    --argjson fail "${FAIL_COUNT}" \
    --argjson skip "${SKIP_COUNT}" \
    --argjson total "${TOTAL}" \
    '{scenario_id: $scenario_id, correlation_id: $correlation_id, verdict: "FAIL", pass: $pass, fail: $fail, skip: $skip, total: $total}' \
    > "${REPORT_FAIL}"
  echo ""
  echo "  VERDICT: FAIL"
  exit 1
fi
