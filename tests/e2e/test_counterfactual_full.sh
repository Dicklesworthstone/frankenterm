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
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Counterfactual Engine Integration E2E ==="

# ── Scenario 1: Override-only ────────────────────────────────────────────
echo ""
echo "--- Scenario 1: Override Loading and Divergence Detection ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_override_only 2>&1 | grep -q "test result: ok"; then
    pass "Override-only divergence detection"
    echo '{"test":"counterfactual_full","scenario":1,"override":"divergence_detected","status":"pass"}'
else
    fail "Override-only divergence detection"
fi

# ── Scenario 2: Fault-only ───────────────────────────────────────────────
echo ""
echo "--- Scenario 2: Fault Injection ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_fault_only 2>&1 | grep -q "test result: ok"; then
    pass "Fault-only graceful degradation"
    echo '{"test":"counterfactual_full","scenario":2,"fault":"pane_death+batch","status":"pass"}'
else
    fail "Fault-only graceful degradation"
fi

# ── Scenario 3: Override + Fault combined ────────────────────────────────
echo ""
echo "--- Scenario 3: Combined Override + Fault ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_combined 2>&1 | grep -q "test result: ok"; then
    pass "Combined override and fault injection"
    echo '{"test":"counterfactual_full","scenario":3,"mode":"combined","status":"pass"}'
else
    fail "Combined override and fault injection"
fi

# ── Scenario 4: Matrix sweep ────────────────────────────────────────────
echo ""
echo "--- Scenario 4: Matrix Sweep ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_matrix 2>&1 | grep -q "test result: ok"; then
    pass "Matrix sweep collects all results"
    echo '{"test":"counterfactual_full","scenario":4,"mode":"matrix","status":"pass"}'
else
    fail "Matrix sweep"
fi

# ── Scenario 5: Guardrail enforcement ────────────────────────────────────
echo ""
echo "--- Scenario 5: Guardrail Enforcement ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration scenario_guardrail 2>&1 | grep -q "test result: ok"; then
    pass "Guardrail enforcement"
    echo '{"test":"counterfactual_full","scenario":5,"mode":"guardrails","status":"pass"}'
else
    fail "Guardrail enforcement"
fi

# ── Scenario 6: Full integration suite ───────────────────────────────────
echo ""
echo "--- Scenario 6: Full Integration Suite ---"

if cargo test -p frankenterm-core --test replay_counterfactual_integration 2>&1 | grep -q "test result: ok"; then
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
