#!/usr/bin/env bash
# E2E smoke test: replay interface parity (ft-og6q6.6.5)
#
# Validates that CLI, Robot Mode, and MCP tool schemas maintain
# consistent behavior and naming conventions.
#
# Summary JSON: {"test":"interface_parity","contract_pass":true,"smoke_pass":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Interface Parity E2E ==="

# ── Scenario 1: Contract tests compile and pass ──────────────────────
echo ""
echo "--- Scenario 1: Interface Contract Tests ---"

if cargo test -p frankenterm-core --test replay_interface_contract 2>&1 | grep -q "test result: ok"; then
    pass "Interface contract tests (42 tests)"
else
    fail "Interface contract tests"
fi

# ── Scenario 2: Proptest suites pass ────────────────────────────────
echo ""
echo "--- Scenario 2: Property-Based Tests ---"

if cargo test -p frankenterm-core --test proptest_replay_mcp 2>&1 | grep -q "test result: ok"; then
    pass "MCP property tests (15 tests)"
else
    fail "MCP property tests"
fi

if cargo test -p frankenterm-core --test proptest_replay_robot 2>&1 | grep -q "test result: ok"; then
    pass "Robot property tests (20 tests)"
else
    fail "Robot property tests"
fi

# ── Scenario 3: Smoke tests (S-01..S-05) ───────────────────────────
echo ""
echo "--- Scenario 3: Smoke Tests ---"

# S-01: Exit code constants are defined
if cargo test -p frankenterm-core --test replay_interface_contract ic33_smoke_exit_code_pass 2>&1 | grep -q "ok"; then
    pass "S-01: Exit code pass=0"
else
    fail "S-01: Exit code pass=0"
fi

# S-02: Default output mode
if cargo test -p frankenterm-core --test replay_interface_contract ic34_smoke_default_output_mode 2>&1 | grep -q "ok"; then
    pass "S-02: Default output mode=Human"
else
    fail "S-02: Default output mode=Human"
fi

# S-03: Minimal artifact inspect
if cargo test -p frankenterm-core --test replay_interface_contract ic35_smoke_inspect_minimal 2>&1 | grep -q "ok"; then
    pass "S-03: Minimal artifact inspect"
else
    fail "S-03: Minimal artifact inspect"
fi

# S-04: Identical diff produces zero divergences
if cargo test -p frankenterm-core --test replay_interface_contract ic36_smoke_diff_identical 2>&1 | grep -q "ok"; then
    pass "S-04: Identical diff = zero divergences"
else
    fail "S-04: Identical diff = zero divergences"
fi

# S-05: Empty artifact list is valid
if cargo test -p frankenterm-core --test replay_interface_contract ic37_smoke_empty_artifact_list 2>&1 | grep -q "ok"; then
    pass "S-05: Empty artifact list valid"
else
    fail "S-05: Empty artifact list valid"
fi

# ── Summary ─────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"interface_parity\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"smoke_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
