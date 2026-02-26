#!/usr/bin/env bash
# E2E: Validate runtime health/doctor surfaces and incident enrichment (ft-e34d9.10.7.2).
#
# Scenarios:
#   1. Rust unit tests compile and pass (cargo test --lib runtime_health)
#   2. Proptest suite compiles and passes
#   3. Source module structure validates (all expected types present)
#   4. Module registered in lib.rs
#   5. Health check built-in functions exist
#   6. Robot type conversion (HealthCheckData, IncidentEnrichmentData) present
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/runtime_health"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_7_2_runtime_health"
CORRELATION_ID="ft-e34d9.10.7.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/runtime_health_contract_${RUN_ID}.jsonl"

PASS=0
FAIL=0
TOTAL=0

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "runtime_health_contract.e2e" \
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

record_result() {
  local name="$1"
  local ok="$2"
  TOTAL=$((TOTAL + 1))
  if [ "$ok" = "true" ]; then
    PASS=$((PASS + 1))
    emit_log "passed" "$name" "scenario_end" "completed" "none" "${LOG_FILE}" ""
    echo "  PASS: $name"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "$name" "scenario_end" "$3" "$4" "${LOG_FILE}" "${5:-}"
    echo "  FAIL: $name"
  fi
}

echo "=== Runtime Health/Doctor Contract E2E (ft-e34d9.10.7.2) ==="
emit_log "started" "e2e_suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

CARGO_TARGET_DIR="${ROOT_DIR}/.target-windymountain-check"
export CARGO_TARGET_DIR

# -----------------------------------------------------------------------
# Scenario 1: Unit tests compile and pass
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 1: Unit tests compile and pass ---"

if cargo test -p frankenterm-core --lib runtime_health -- --nocapture \
    2>"${ARTIFACT_DIR}/unit_test_stderr.log" \
    >"${ARTIFACT_DIR}/unit_test_stdout.log"; then
  test_count=$(grep -c 'test runtime_health::tests::' "${ARTIFACT_DIR}/unit_test_stdout.log" || echo "0")
  record_result "unit_tests_pass" "true"
  echo "    ${test_count} tests passed"
else
  record_result "unit_tests_pass" "false" "assertion_failed" "assertion_failed" "see unit_test_stderr.log"
fi

# -----------------------------------------------------------------------
# Scenario 2: Proptest suite compiles and passes
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 2: Proptest suite compiles and passes ---"

if cargo test -p frankenterm-core --test proptest_runtime_health -- --nocapture \
    2>"${ARTIFACT_DIR}/proptest_stderr.log" \
    >"${ARTIFACT_DIR}/proptest_stdout.log"; then
  record_result "proptests_pass" "true"
else
  record_result "proptests_pass" "false" "assertion_failed" "assertion_failed" "see proptest_stderr.log"
fi

# -----------------------------------------------------------------------
# Scenario 3: Source module structure
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 3: Source module structure ---"

MODULE_FILE="${ROOT_DIR}/crates/frankenterm-core/src/runtime_health.rs"
STRUCTURE_OK="true"

for type_name in CheckStatus RemediationHint RemediationEffort RuntimeHealthCheck \
                 RuntimeDoctorReport StatusCounts HealthCheckRegistry \
                 IncidentEnrichment ActiveFailure \
                 HealthCheckData HealthCheckItem RemediationItem HealthSummary \
                 IncidentEnrichmentData; do
  if ! grep -q "${type_name}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing type: ${type_name}"
    STRUCTURE_OK="false"
  fi
done

# Check built-in health check functions
for func_name in check_telemetry_log check_tier_distribution check_scope_health check_failure_patterns; do
  if ! grep -q "pub fn ${func_name}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing function: ${func_name}"
    STRUCTURE_OK="false"
  fi
done

if [ "$STRUCTURE_OK" = "true" ]; then
  record_result "module_structure" "true"
else
  record_result "module_structure" "false" "precondition_failed" "config" "missing types/functions"
fi

# -----------------------------------------------------------------------
# Scenario 4: Module registered in lib.rs
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 4: Module registered in lib.rs ---"

LIB_FILE="${ROOT_DIR}/crates/frankenterm-core/src/lib.rs"
if grep -q 'pub mod runtime_health;' "${LIB_FILE}" 2>/dev/null; then
  record_result "module_registered" "true"
else
  record_result "module_registered" "false" "precondition_failed" "config" "module not in lib.rs"
fi

# -----------------------------------------------------------------------
# Scenario 5: Integration with runtime_telemetry
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 5: runtime_telemetry integration ---"

INTEGRATION_OK="true"
# Should use HealthTier, RuntimePhase, FailureClass from runtime_telemetry
for import in "HealthTier" "RuntimePhase" "FailureClass" "RuntimeTelemetryLog"; do
  if ! grep -q "${import}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing integration reference: ${import}"
    INTEGRATION_OK="false"
  fi
done

if [ "$INTEGRATION_OK" = "true" ]; then
  record_result "telemetry_integration" "true"
else
  record_result "telemetry_integration" "false" "precondition_failed" "config" "missing integration"
fi

# -----------------------------------------------------------------------
# Scenario 6: Serde snake_case compliance
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 6: Serde snake_case compliance ---"

SERDE_OK="true"
for enum_name in CheckStatus RemediationEffort; do
  if ! grep -B3 "pub enum ${enum_name}" "${MODULE_FILE}" | grep -q 'rename_all = "snake_case"'; then
    echo "    ${enum_name} missing serde snake_case"
    SERDE_OK="false"
  fi
done

if [ "$SERDE_OK" = "true" ]; then
  record_result "serde_compliance" "true"
else
  record_result "serde_compliance" "false" "invariant_violation" "assertion_failed" "missing serde"
fi

# -----------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------
echo ""
echo "=== Summary ==="
echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
echo "  Log: ${LOG_FILE}"

emit_log "$([ "$FAIL" -eq 0 ] && echo passed || echo failed)" \
  "e2e_suite" "script_end" "completed" "none" "${LOG_FILE}" \
  "total=${TOTAL},pass=${PASS},fail=${FAIL}"

jq -csn \
  --arg test "runtime_health_contract" \
  --argjson scenarios_pass "${PASS}" \
  --argjson scenarios_fail "${FAIL}" \
  --argjson total "${TOTAL}" \
  --arg log_file "${LOG_FILE}" \
  '{
    test: $test,
    scenarios_pass: $scenarios_pass,
    scenarios_fail: $scenarios_fail,
    total: $total,
    log_file: $log_file
  }'

[ "$FAIL" -eq 0 ]
