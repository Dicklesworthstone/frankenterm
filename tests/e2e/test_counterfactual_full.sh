#!/usr/bin/env bash
# E2E smoke test: counterfactual engine integration (ft-og6q6.4.5)
#
# Validates override loading, fault injection, matrix execution,
# and guardrail enforcement using Rust integration tests as ground truth.
#
# Summary JSON: {"test":"counterfactual_full","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-counterfactual-full-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/counterfactual_full_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/counterfactual_full_${RUN_ID}.smoke.log"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }
fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift

    set +e
    (
        cd "${REPO_ROOT}"
        run_rch_cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi

    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi

    set +e
    run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
    local smoke_rc=$?
    set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

echo "=== Counterfactual Engine Integration E2E ==="
ensure_rch_ready

# ── Scenario 1: Override-only ────────────────────────────────────────────
echo ""
echo "--- Scenario 1: Override Loading and Divergence Detection ---"

scenario1_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --test replay_counterfactual_integration scenario_override_only \
    && grep -q "test result: ok" "${scenario1_log}"; then
    pass "Override-only divergence detection"
    echo '{"test":"counterfactual_full","scenario":1,"override":"divergence_detected","status":"pass"}'
else
    fail "Override-only divergence detection"
fi

# ── Scenario 2: Fault-only ───────────────────────────────────────────────
echo ""
echo "--- Scenario 2: Fault Injection ---"

scenario2_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --test replay_counterfactual_integration scenario_fault_only \
    && grep -q "test result: ok" "${scenario2_log}"; then
    pass "Fault-only graceful degradation"
    echo '{"test":"counterfactual_full","scenario":2,"fault":"pane_death+batch","status":"pass"}'
else
    fail "Fault-only graceful degradation"
fi

# ── Scenario 3: Override + Fault combined ────────────────────────────────
echo ""
echo "--- Scenario 3: Combined Override + Fault ---"

scenario3_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --test replay_counterfactual_integration scenario_combined \
    && grep -q "test result: ok" "${scenario3_log}"; then
    pass "Combined override and fault injection"
    echo '{"test":"counterfactual_full","scenario":3,"mode":"combined","status":"pass"}'
else
    fail "Combined override and fault injection"
fi

# ── Scenario 4: Matrix sweep ────────────────────────────────────────────
echo ""
echo "--- Scenario 4: Matrix Sweep ---"

scenario4_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --test replay_counterfactual_integration scenario_matrix \
    && grep -q "test result: ok" "${scenario4_log}"; then
    pass "Matrix sweep collects all results"
    echo '{"test":"counterfactual_full","scenario":4,"mode":"matrix","status":"pass"}'
else
    fail "Matrix sweep"
fi

# ── Scenario 5: Guardrail enforcement ────────────────────────────────────
echo ""
echo "--- Scenario 5: Guardrail Enforcement ---"

scenario5_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --test replay_counterfactual_integration scenario_guardrail \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "Guardrail enforcement"
    echo '{"test":"counterfactual_full","scenario":5,"mode":"guardrails","status":"pass"}'
else
    fail "Guardrail enforcement"
fi

# ── Scenario 6: Full integration suite ───────────────────────────────────
echo ""
echo "--- Scenario 6: Full Integration Suite ---"

scenario6_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" test -p frankenterm-core --test replay_counterfactual_integration \
    && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All counterfactual integration tests (24 tests)"
else
    fail "Full integration suite"
fi

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"counterfactual_full\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
