#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_2_mux_migration_completion"
CORRELATION_ID="ft-e34d9.10.5.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mux_migration_completion_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1" decision_path="$2" reason_code="$3" error_code="$4" input_summary="$5"
  local ts; ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg ts "${ts}" --arg component "mux_migration_completion.e2e" \
    --arg sid "${SCENARIO_ID}" --arg cid "${CORRELATION_ID}" \
    --arg dp "${decision_path}" --arg is "${input_summary}" \
    --arg oc "${outcome}" --arg rc "${reason_code}" --arg ec "${error_code}" \
    '{timestamp:$ts,component:$component,scenario_id:$sid,correlation_id:$cid,
      decision_path:$dp,input_summary:$is,outcome:$oc,reason_code:$rc,error_code:$ec}' \
    >> "${LOG_FILE}"
}

echo "=== Mux migration completion validation (ft-e34d9.10.5.2) ==="
echo "Run ID: ${RUN_ID}"
echo "Log:    ${LOG_FILE_REL}"
echo ""

PASS=0; FAIL=0

# S1: Integration test file exists
echo -n "S1: Integration test file exists... "
if [ -f "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" ]; then
  echo "PASS"; emit_log "pass" "integration_test_file" "exists" "" ""; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "integration_test_file" "missing" "E_FILE" ""; FAIL=$((FAIL+1))
fi

# S2: Integration test gated on vendored+asupersync
echo -n "S2: Test feature-gated correctly... "
GATE=$(head -10 "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" \
  | grep -c 'cfg(all(feature = "asupersync-runtime", feature = "vendored"' || true)
if [ "${GATE}" -ge 1 ]; then
  echo "PASS"; emit_log "pass" "feature_gate" "correct" "" ""; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "feature_gate" "missing" "E_GATE" ""; FAIL=$((FAIL+1))
fi

# S3: Test count >= 10
echo -n "S3: Integration test count... "
TEST_COUNT=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" || true)
if [ "${TEST_COUNT}" -ge 10 ]; then
  echo "PASS (${TEST_COUNT} tests)"; emit_log "pass" "test_count" "sufficient" "" "count=${TEST_COUNT}"; PASS=$((PASS+1))
else
  echo "FAIL (${TEST_COUNT})"; emit_log "fail" "test_count" "insufficient" "E_TESTS" "count=${TEST_COUNT}"; FAIL=$((FAIL+1))
fi

# S4: Cancellation tests present
echo -n "S4: Cancellation tests present... "
CANCEL_TESTS=$(grep -c 'cancelled_cx\|timeout_cx\|user_cancelled' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" || true)
if [ "${CANCEL_TESTS}" -ge 3 ]; then
  echo "PASS (${CANCEL_TESTS} cancellation refs)"; emit_log "pass" "cancellation_tests" "present" "" "refs=${CANCEL_TESTS}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "cancellation_tests" "missing" "E_CANCEL" ""; FAIL=$((FAIL+1))
fi

# S5: Concurrent operation tests present
echo -n "S5: Concurrent tests present... "
CONCURRENT=$(grep -c 'task::spawn\|concurrent' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" || true)
if [ "${CONCURRENT}" -ge 3 ]; then
  echo "PASS"; emit_log "pass" "concurrent_tests" "present" "" "refs=${CONCURRENT}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "concurrent_tests" "missing" "E_CONCURRENT" ""; FAIL=$((FAIL+1))
fi

# S6: Fault injection tests present
echo -n "S6: Fault injection tests present... "
FAULT=$(grep -c 'SimulatedNetwork\|fault_injection\|lossy\|hostile' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" || true)
if [ "${FAULT}" -ge 2 ]; then
  echo "PASS (${FAULT} fault refs)"; emit_log "pass" "fault_injection" "present" "" "refs=${FAULT}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "fault_injection" "missing" "E_FAULT" ""; FAIL=$((FAIL+1))
fi

# S7: Serde roundtrip test for diagnostics
echo -n "S7: Serde roundtrip test... "
SERDE=$(grep -c 'serde_json\|MuxPoolStats' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs" || true)
if [ "${SERDE}" -ge 2 ]; then
  echo "PASS"; emit_log "pass" "serde_test" "present" "" "refs=${SERDE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "serde_test" "missing" "E_SERDE" ""; FAIL=$((FAIL+1))
fi

# S8: All 63 mux_pool unit tests in-module (count check)
echo -n "S8: mux_pool unit test count... "
MUX_UNIT=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${MUX_UNIT}" -ge 60 ]; then
  echo "PASS (${MUX_UNIT} tests)"; emit_log "pass" "mux_pool_unit_tests" "sufficient" "" "count=${MUX_UNIT}"; PASS=$((PASS+1))
else
  echo "FAIL (${MUX_UNIT})"; emit_log "fail" "mux_pool_unit_tests" "insufficient" "E_UNIT" "count=${MUX_UNIT}"; FAIL=$((FAIL+1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

[ "${FAIL}" -gt 0 ] && exit 1 || exit 0
