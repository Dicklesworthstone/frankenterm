#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_3_6_swarm_simulation_regression"
CORRELATION_ID="ft-3681t.3.6-${RUN_ID}"
LOG_FILE="${LOG_DIR}/swarm_simulation_regression_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1" decision_path="$2" reason_code="$3" error_code="$4" input_summary="$5"
  local ts; ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg ts "${ts}" --arg component "swarm_simulation_regression.e2e" \
    --arg sid "${SCENARIO_ID}" --arg cid "${CORRELATION_ID}" \
    --arg dp "${decision_path}" --arg is "${input_summary}" \
    --arg oc "${outcome}" --arg rc "${reason_code}" --arg ec "${error_code}" \
    '{timestamp:$ts,component:$component,scenario_id:$sid,correlation_id:$cid,
      decision_path:$dp,input_summary:$is,outcome:$oc,reason_code:$rc,error_code:$ec}' \
    >> "${LOG_FILE}"
}

echo "=== Swarm simulation and regression suite validation (ft-3681t.3.6) ==="
echo "Run ID: ${RUN_ID}"
echo "Log:    ${LOG_FILE_REL}"
echo ""

PASS=0; FAIL=0

# S1: Simulation test file exists
echo -n "S1: Simulation test file exists... "
if [ -f "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" ]; then
  echo "PASS"; emit_log "pass" "sim_test_file" "exists" "" ""; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "sim_test_file" "missing" "E_FILE" ""; FAIL=$((FAIL+1))
fi

# S2: Test count >= 25
echo -n "S2: Simulation test count... "
TEST_COUNT=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${TEST_COUNT}" -ge 25 ]; then
  echo "PASS (${TEST_COUNT} tests)"; emit_log "pass" "test_count" "sufficient" "" "count=${TEST_COUNT}"; PASS=$((PASS+1))
else
  echo "FAIL (${TEST_COUNT})"; emit_log "fail" "test_count" "insufficient" "E_TESTS" "count=${TEST_COUNT}"; FAIL=$((FAIL+1))
fi

# S3: Load spike simulations present
echo -n "S3: Load spike simulations... "
SPIKE=$(grep -c 'sim_burst_enqueue\|sim_sustained_throughput\|sim_load_spike\|sim_post_spike\|sim_deep_dag' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${SPIKE}" -ge 5 ]; then
  echo "PASS (${SPIKE} spike tests)"; emit_log "pass" "spike_sims" "present" "" "refs=${SPIKE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "spike_sims" "missing" "E_SPIKE" "refs=${SPIKE}"; FAIL=$((FAIL+1))
fi

# S4: Agent failure simulations present
echo -n "S4: Agent failure simulations... "
FAILURE=$(grep -c 'sim_agent_failure\|sim_heartbeat\|sim_multi_agent_failure\|sim_high_failure\|sim_cascading' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${FAILURE}" -ge 5 ]; then
  echo "PASS (${FAILURE} failure tests)"; emit_log "pass" "failure_sims" "present" "" "refs=${FAILURE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "failure_sims" "missing" "E_FAILURE" "refs=${FAILURE}"; FAIL=$((FAIL+1))
fi

# S5: Recovery simulations present
echo -n "S5: Recovery simulations... "
RECOVERY=$(grep -c 'sim_snapshot_restore\|sim_scheduler_snapshot\|sim_reassignment\|sim_full_recovery\|sim_beads_import' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${RECOVERY}" -ge 5 ]; then
  echo "PASS (${RECOVERY} recovery tests)"; emit_log "pass" "recovery_sims" "present" "" "refs=${RECOVERY}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "recovery_sims" "missing" "E_RECOVERY" "refs=${RECOVERY}"; FAIL=$((FAIL+1))
fi

# S6: Decision quality metrics present
echo -n "S6: Quality metrics (fairness/throughput/stability)... "
METRICS=$(grep -c 'gini_coefficient\|sim_throughput\|sim_scheduler_decision_stability\|sim_queue_pressure' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${METRICS}" -ge 5 ]; then
  echo "PASS (${METRICS} metric refs)"; emit_log "pass" "quality_metrics" "present" "" "refs=${METRICS}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "quality_metrics" "missing" "E_METRICS" "refs=${METRICS}"; FAIL=$((FAIL+1))
fi

# S7: Regression anchors present
echo -n "S7: Regression anchors... "
REGR=$(grep -c 'regression_queue_stats\|regression_completed_items\|regression_priority\|regression_cycle\|regression_ownership' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${REGR}" -ge 5 ]; then
  echo "PASS (${REGR} regression tests)"; emit_log "pass" "regression_anchors" "present" "" "refs=${REGR}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "regression_anchors" "missing" "E_REGR" "refs=${REGR}"; FAIL=$((FAIL+1))
fi

# S8: Pipeline simulations present
echo -n "S8: Pipeline failure/recovery simulations... "
PIPE=$(grep -c 'sim_pipeline_multi\|sim_pipeline_exponential\|sim_pipeline_hook\|sim_pipeline_optional\|sim_pipeline_large' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${PIPE}" -ge 5 ]; then
  echo "PASS (${PIPE} pipeline tests)"; emit_log "pass" "pipeline_sims" "present" "" "refs=${PIPE}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "pipeline_sims" "missing" "E_PIPELINE" "refs=${PIPE}"; FAIL=$((FAIL+1))
fi

# S9: Structured logging present
echo -n "S9: Structured logging in simulations... "
LOGS=$(grep -c 'emit_sim_log' "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_simulation_regression.rs" || true)
if [ "${LOGS}" -ge 20 ]; then
  echo "PASS (${LOGS} log emits)"; emit_log "pass" "structured_logging" "present" "" "refs=${LOGS}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "structured_logging" "insufficient" "E_LOG" "refs=${LOGS}"; FAIL=$((FAIL+1))
fi

# S10: Existing integration tests still present
echo -n "S10: Existing integration tests intact... "
EXISTING=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/tests/swarm_orchestration_integration.rs" || true)
if [ "${EXISTING}" -ge 16 ]; then
  echo "PASS (${EXISTING} existing tests)"; emit_log "pass" "existing_tests" "intact" "" "count=${EXISTING}"; PASS=$((PASS+1))
else
  echo "FAIL"; emit_log "fail" "existing_tests" "degraded" "E_EXISTING" "count=${EXISTING}"; FAIL=$((FAIL+1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

[ "${FAIL}" -gt 0 ] && exit 1 || exit 0
