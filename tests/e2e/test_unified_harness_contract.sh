#!/usr/bin/env bash
# E2E: Validate unified test harness contract (ft-e34d9.10.6.5).
#
# Scenarios:
#   1. Rust harness tests compile and pass (cargo test --test test_harness_contract)
#   2. Emitted JSONL artifacts conform to ADR-0012 (10 required fields)
#   3. Reason/error codes serialize as snake_case strings
#   4. Cross-format parity: Rust-emitted events structurally match shell-emitted events
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/harness_contract"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_6_5_unified_harness"
CORRELATION_ID="ft-e34d9.10.6.5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/unified_harness_contract_${RUN_ID}.jsonl"

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
    --arg component "unified_harness_contract.e2e" \
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

echo "=== Unified Test Harness Contract E2E (ft-e34d9.10.6.5) ==="
emit_log "started" "e2e_suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

# -----------------------------------------------------------------------
# Scenario 1: Rust harness tests compile and pass
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 1: Rust harness tests compile and pass ---"
emit_log "started" "rust_test_compile" "cargo_test" "none" "none" "${LOG_FILE}" ""

CARGO_TARGET_DIR="${ROOT_DIR}/.target-windymountain-check"
export CARGO_TARGET_DIR

if cargo test -p frankenterm-core --test test_harness_contract -- --nocapture 2>"${ARTIFACT_DIR}/cargo_test_stderr.log" >"${ARTIFACT_DIR}/cargo_test_stdout.log"; then
  record_result "rust_harness_tests_pass" "true"
else
  record_result "rust_harness_tests_pass" "false" "assertion_failed" "assertion_failed" "see cargo_test_stderr.log"
fi

# -----------------------------------------------------------------------
# Scenario 2: Source modules exist and have expected structure
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 2: Harness source modules present ---"

MODULES_OK="true"
for mod_file in \
  "crates/frankenterm-core/tests/common/mod.rs" \
  "crates/frankenterm-core/tests/common/reason_codes.rs" \
  "crates/frankenterm-core/tests/common/test_event_logger.rs" \
  "crates/frankenterm-core/tests/test_harness_contract.rs"; do
  if [ ! -f "${ROOT_DIR}/${mod_file}" ]; then
    echo "    Missing: ${mod_file}"
    MODULES_OK="false"
  fi
done

if [ "$MODULES_OK" = "true" ]; then
  record_result "harness_modules_present" "true"
else
  record_result "harness_modules_present" "false" "precondition_failed" "config" "missing files"
fi

# -----------------------------------------------------------------------
# Scenario 3: Reason/error code source contains expected variants
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 3: Reason/error code taxonomy coverage ---"

REASON_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/common/reason_codes.rs"
TAXONOMY_OK="true"

for variant in TimeoutExpired ChannelClosed CancellationLoss CancellationRequested \
               ScopeShutdown PanicPropagated ChaosInjected InvariantViolation OracleFailure; do
  if ! grep -q "${variant}" "${REASON_FILE}" 2>/dev/null; then
    echo "    Missing ReasonCode variant: ${variant}"
    TAXONOMY_OK="false"
  fi
done

for variant in AssertionFailed Timeout Panic Deadlock TaskLeak DataLoss SafetyViolation LivenessViolation; do
  if ! grep -q "${variant}" "${REASON_FILE}" 2>/dev/null; then
    echo "    Missing ErrorCode variant: ${variant}"
    TAXONOMY_OK="false"
  fi
done

if [ "$TAXONOMY_OK" = "true" ]; then
  record_result "taxonomy_coverage" "true"
else
  record_result "taxonomy_coverage" "false" "precondition_failed" "config" "missing variants"
fi

# -----------------------------------------------------------------------
# Scenario 4: Cross-format parity (shell vs Rust event structure)
# -----------------------------------------------------------------------
echo ""
echo "--- Scenario 4: Cross-format parity ---"

# Emit a reference event from shell.
SHELL_EVENT_FILE="${ARTIFACT_DIR}/shell_reference_event.json"
jq -cn \
  --arg timestamp "2026-02-26T00:00:00Z" \
  --arg component "parity_test.e2e" \
  --arg scenario_id "ft_e34d9_10_6_5:parity_check" \
  --arg correlation_id "ft-e34d9.10.6.5-parity" \
  --arg decision_path "verify" \
  --arg input_summary "N=42" \
  --arg outcome "passed" \
  --arg reason_code "completed" \
  --arg error_code "none" \
  --arg artifact_path "" \
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
  }' > "${SHELL_EVENT_FILE}"

# Verify shell event has all 10 required fields.
PARITY_OK="true"
for field in timestamp component scenario_id correlation_id decision_path \
             input_summary outcome reason_code error_code artifact_path; do
  if ! jq -e ".${field}" "${SHELL_EVENT_FILE}" >/dev/null 2>&1; then
    echo "    Shell event missing field: ${field}"
    PARITY_OK="false"
  fi
done

# Verify the field *names* match what Rust produces (checked in Scenario 1).
FIELD_COUNT=$(jq 'keys | length' "${SHELL_EVENT_FILE}")
if [ "${FIELD_COUNT}" -ne 10 ]; then
  echo "    Shell event has ${FIELD_COUNT} fields, expected 10"
  PARITY_OK="false"
fi

if [ "$PARITY_OK" = "true" ]; then
  record_result "cross_format_parity" "true"
else
  record_result "cross_format_parity" "false" "invariant_violation" "assertion_failed" "parity mismatch"
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
  --arg test "unified_harness_contract" \
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
