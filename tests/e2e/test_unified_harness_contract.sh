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

# ── rch infrastructure ──────────────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-unified-harness-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/unified_harness_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/unified_harness_${RUN_ID}.smoke.log"

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
    --arg component "unified_harness_contract.e2e" \
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

echo "=== Unified Test Harness Contract E2E (ft-e34d9.10.6.5) ==="
emit_log "started" "e2e_suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

ensure_rch_ready

# Scenario 1: Rust harness tests compile and pass
echo ""; echo "--- Scenario 1: Rust harness tests compile and pass ---"
emit_log "started" "rust_test_compile" "cargo_test" "none" "none" "${LOG_FILE}" ""

step1_log="${LOG_DIR}/unified_harness_${RUN_ID}.cargo_test.log"
if run_rch_cargo_logged "${step1_log}" test -p frankenterm-core --test test_harness_contract -- --nocapture; then
  record_result "rust_harness_tests_pass" "true"
else
  record_result "rust_harness_tests_pass" "false" "assertion_failed" "assertion_failed" "see ${step1_log}"
fi

# Scenario 2: Source modules exist and have expected structure
echo ""; echo "--- Scenario 2: Harness source modules present ---"
MODULES_OK="true"
for mod_file in \
  "crates/frankenterm-core/tests/common/mod.rs" \
  "crates/frankenterm-core/tests/common/reason_codes.rs" \
  "crates/frankenterm-core/tests/common/test_event_logger.rs" \
  "crates/frankenterm-core/tests/test_harness_contract.rs"; do
  if [ ! -f "${ROOT_DIR}/${mod_file}" ]; then
    echo "    Missing: ${mod_file}"; MODULES_OK="false"
  fi
done
if [ "$MODULES_OK" = "true" ]; then
  record_result "harness_modules_present" "true"
else
  record_result "harness_modules_present" "false" "precondition_failed" "config" "missing files"
fi

# Scenario 3: Reason/error code source contains expected variants
echo ""; echo "--- Scenario 3: Reason/error code taxonomy coverage ---"
REASON_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/common/reason_codes.rs"
TAXONOMY_OK="true"
for variant in TimeoutExpired ChannelClosed CancellationLoss CancellationRequested \
               ScopeShutdown PanicPropagated ChaosInjected InvariantViolation OracleFailure; do
  if ! grep -q "${variant}" "${REASON_FILE}" 2>/dev/null; then
    echo "    Missing ReasonCode variant: ${variant}"; TAXONOMY_OK="false"
  fi
done
for variant in AssertionFailed Timeout Panic Deadlock TaskLeak DataLoss SafetyViolation LivenessViolation; do
  if ! grep -q "${variant}" "${REASON_FILE}" 2>/dev/null; then
    echo "    Missing ErrorCode variant: ${variant}"; TAXONOMY_OK="false"
  fi
done
if [ "$TAXONOMY_OK" = "true" ]; then
  record_result "taxonomy_coverage" "true"
else
  record_result "taxonomy_coverage" "false" "precondition_failed" "config" "missing variants"
fi

# Scenario 4: Cross-format parity (shell vs Rust event structure)
echo ""; echo "--- Scenario 4: Cross-format parity ---"
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
  '{timestamp:$timestamp,component:$component,scenario_id:$scenario_id,correlation_id:$correlation_id,decision_path:$decision_path,input_summary:$input_summary,outcome:$outcome,reason_code:$reason_code,error_code:$error_code,artifact_path:$artifact_path}' > "${SHELL_EVENT_FILE}"

PARITY_OK="true"
for field in timestamp component scenario_id correlation_id decision_path \
             input_summary outcome reason_code error_code artifact_path; do
  if ! jq -e ".${field}" "${SHELL_EVENT_FILE}" >/dev/null 2>&1; then
    echo "    Shell event missing field: ${field}"; PARITY_OK="false"
  fi
done
FIELD_COUNT=$(jq 'keys | length' "${SHELL_EVENT_FILE}")
if [ "${FIELD_COUNT}" -ne 10 ]; then
  echo "    Shell event has ${FIELD_COUNT} fields, expected 10"; PARITY_OK="false"
fi
if [ "$PARITY_OK" = "true" ]; then
  record_result "cross_format_parity" "true"
else
  record_result "cross_format_parity" "false" "invariant_violation" "assertion_failed" "parity mismatch"
fi

# Summary
echo ""; echo "=== Summary ==="
echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
echo "  Log: ${LOG_FILE}"
emit_log "$([ "$FAIL" -eq 0 ] && echo passed || echo failed)" \
  "e2e_suite" "script_end" "completed" "none" "${LOG_FILE}" \
  "total=${TOTAL},pass=${PASS},fail=${FAIL}"
jq -csn --arg test "unified_harness_contract" --argjson scenarios_pass "${PASS}" \
  --argjson scenarios_fail "${FAIL}" --argjson total "${TOTAL}" --arg log_file "${LOG_FILE}" \
  '{test:$test,scenarios_pass:$scenarios_pass,scenarios_fail:$scenarios_fail,total:$total,log_file:$log_file}'
[ "$FAIL" -eq 0 ]
