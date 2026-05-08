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
GUARD_LIB="${REPO_ROOT}/tests/e2e/lib_rch_guards.sh"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-counterfactual-full-${RUN_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "counterfactual_full" "${REPO_ROOT}"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Counterfactual Engine Integration E2E ==="
ensure_rch_ready

# ── Scenario 1: Override-only ────────────────────────────────────────────
echo ""
echo "--- Scenario 1: Override Loading and Divergence Detection ---"

scenario1_log="${LOG_DIR}/counterfactual_full_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_override_only \
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
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_fault_only \
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
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_combined \
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
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_matrix \
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
if run_rch_cargo_logged "${scenario5_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_guardrail \
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
if run_rch_cargo_logged "${scenario6_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test replay_counterfactual_integration \
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
