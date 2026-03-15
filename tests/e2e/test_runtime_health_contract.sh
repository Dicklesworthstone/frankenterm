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

# ── rch infrastructure ──────────────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-runtime-health-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/runtime_health_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/runtime_health_${RUN_ID}.smoke.log"

fatal() { echo "FATAL: $1" >&2; exit 1; }
run_rch() { TMPDIR=/tmp rch "$@"; }
run_rch_cargo() { run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"; }
probe_has_reachable_workers() { grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"; }

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"; shift
    set +e; ( cd "${ROOT_DIR}"; run_rch_cargo "$@" ) >"${output_file}" 2>&1; local rc=$?; set -e
    check_rch_fallback "${output_file}"; return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e; run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1; local probe_rc=$?; set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e; run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1; local smoke_rc=$?; set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed. See ${RCH_SMOKE_LOG}"
    fi
}

emit_log() {
  local outcome="$1" scenario="$2" decision_path="$3" reason_code="$4"
  local error_code="$5" artifact_path="$6" input_summary="$7"
  local ts; ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
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
    '{timestamp:$timestamp,component:$component,scenario_id:$scenario_id,correlation_id:$correlation_id,decision_path:$decision_path,input_summary:$input_summary,outcome:$outcome,reason_code:$reason_code,error_code:$error_code,artifact_path:$artifact_path}' >> "${LOG_FILE}"
}

record_result() {
  local name="$1" ok="$2"
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

ensure_rch_ready

# Scenario 1: Unit tests compile and pass
echo ""; echo "--- Scenario 1: Unit tests compile and pass ---"
step1_log="${LOG_DIR}/runtime_health_${RUN_ID}.unit.log"
if run_rch_cargo_logged "${step1_log}" test -p frankenterm-core --lib runtime_health -- --nocapture; then
  test_count=$(grep -c 'test runtime_health::tests::' "${step1_log}" || echo "0")
  record_result "unit_tests_pass" "true"
  echo "    ${test_count} tests passed"
else
  record_result "unit_tests_pass" "false" "assertion_failed" "assertion_failed" "see ${step1_log}"
fi

# Scenario 2: Proptest suite compiles and passes
echo ""; echo "--- Scenario 2: Proptest suite compiles and passes ---"
step2_log="${LOG_DIR}/runtime_health_${RUN_ID}.proptest.log"
if run_rch_cargo_logged "${step2_log}" test -p frankenterm-core --test proptest_runtime_health -- --nocapture; then
  record_result "proptests_pass" "true"
else
  record_result "proptests_pass" "false" "assertion_failed" "assertion_failed" "see ${step2_log}"
fi

# Scenario 3: Source module structure
echo ""; echo "--- Scenario 3: Source module structure ---"
MODULE_FILE="${ROOT_DIR}/crates/frankenterm-core/src/runtime_health.rs"
STRUCTURE_OK="true"
for type_name in CheckStatus RemediationHint RemediationEffort RuntimeHealthCheck \
                 RuntimeDoctorReport StatusCounts HealthCheckRegistry \
                 IncidentEnrichment ActiveFailure \
                 HealthCheckData HealthCheckItem RemediationItem HealthSummary \
                 IncidentEnrichmentData; do
  if ! grep -q "${type_name}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing type: ${type_name}"; STRUCTURE_OK="false"
  fi
done
for func_name in check_telemetry_log check_tier_distribution check_scope_health check_failure_patterns; do
  if ! grep -q "pub fn ${func_name}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing function: ${func_name}"; STRUCTURE_OK="false"
  fi
done
if [ "$STRUCTURE_OK" = "true" ]; then
  record_result "module_structure" "true"
else
  record_result "module_structure" "false" "precondition_failed" "config" "missing types/functions"
fi

# Scenario 4: Module registered in lib.rs
echo ""; echo "--- Scenario 4: Module registered in lib.rs ---"
LIB_FILE="${ROOT_DIR}/crates/frankenterm-core/src/lib.rs"
if grep -q 'pub mod runtime_health;' "${LIB_FILE}" 2>/dev/null; then
  record_result "module_registered" "true"
else
  record_result "module_registered" "false" "precondition_failed" "config" "module not in lib.rs"
fi

# Scenario 5: Integration with runtime_telemetry
echo ""; echo "--- Scenario 5: runtime_telemetry integration ---"
INTEGRATION_OK="true"
for import in "HealthTier" "RuntimePhase" "FailureClass" "RuntimeTelemetryLog"; do
  if ! grep -q "${import}" "${MODULE_FILE}" 2>/dev/null; then
    echo "    Missing integration reference: ${import}"; INTEGRATION_OK="false"
  fi
done
if [ "$INTEGRATION_OK" = "true" ]; then
  record_result "telemetry_integration" "true"
else
  record_result "telemetry_integration" "false" "precondition_failed" "config" "missing integration"
fi

# Scenario 6: Serde snake_case compliance
echo ""; echo "--- Scenario 6: Serde snake_case compliance ---"
SERDE_OK="true"
for enum_name in CheckStatus RemediationEffort; do
  if ! grep -B3 "pub enum ${enum_name}" "${MODULE_FILE}" | grep -q 'rename_all = "snake_case"'; then
    echo "    ${enum_name} missing serde snake_case"; SERDE_OK="false"
  fi
done
if [ "$SERDE_OK" = "true" ]; then
  record_result "serde_compliance" "true"
else
  record_result "serde_compliance" "false" "invariant_violation" "assertion_failed" "missing serde"
fi

# Summary
echo ""; echo "=== Summary ==="
echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
echo "  Log: ${LOG_FILE}"
emit_log "$([ "$FAIL" -eq 0 ] && echo passed || echo failed)" \
  "e2e_suite" "script_end" "completed" "none" "${LOG_FILE}" \
  "total=${TOTAL},pass=${PASS},fail=${FAIL}"
jq -csn --arg test "runtime_health_contract" --argjson scenarios_pass "${PASS}" \
  --argjson scenarios_fail "${FAIL}" --argjson total "${TOTAL}" --arg log_file "${LOG_FILE}" \
  '{test:$test,scenarios_pass:$scenarios_pass,scenarios_fail:$scenarios_fail,total:$total,log_file:$log_file}'
[ "$FAIL" -eq 0 ]
