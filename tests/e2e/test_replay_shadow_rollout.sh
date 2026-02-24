#!/usr/bin/env bash
# E2E smoke test: replay shadow rollout (ft-og6q6.7.5)
#
# Validates shadow mode, enforcement, kill switch, and flaky detection
# using the Rust module as ground truth.
#
# Summary JSON: {"test":"shadow_rollout","scenario":N,"mode":"shadow|enforce|killswitch",
#                "gate_result":"pass|fail","pr_blocked":true|false,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Shadow Rollout E2E ==="

# ── Scenario 1: Shadow mode, gate fails, PR not blocked ───────────────
echo ""
echo "--- Scenario 1: Shadow Mode ---"

if cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::shadow_mode_fail_not_blocked 2>&1 | grep -q "ok"; then
    pass "Shadow mode: fail not blocked"
    echo '{"test":"shadow_rollout","scenario":1,"mode":"shadow","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Shadow mode: fail not blocked"
fi

# ── Scenario 2: Enforcement mode, gate fails, PR blocked ─────────────
echo ""
echo "--- Scenario 2: Enforcement Mode ---"

if cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::enforce_mode_fail_blocked 2>&1 | grep -q "ok"; then
    pass "Enforce mode: fail blocked"
    echo '{"test":"shadow_rollout","scenario":2,"mode":"enforce","gate_result":"fail","pr_blocked":true,"status":"pass"}'
else
    fail "Enforce mode: fail blocked"
fi

# ── Scenario 3: Kill switch, no enforcement ───────────────────────────
echo ""
echo "--- Scenario 3: Kill Switch ---"

if cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::kill_switch_overrides_enforcement 2>&1 | grep -q "ok"; then
    pass "Kill switch overrides"
    echo '{"test":"shadow_rollout","scenario":3,"mode":"killswitch","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Kill switch overrides"
fi

# ── Scenario 4: Flaky test detection ──────────────────────────────────
echo ""
echo "--- Scenario 4: Flaky Detection ---"

if cargo test -p frankenterm-core --lib replay_shadow_rollout::tests::flaky_rate_critical 2>&1 | grep -q "ok"; then
    pass "Flaky rate detection"
    echo '{"test":"shadow_rollout","scenario":4,"mode":"shadow","gate_result":"flaky","pr_blocked":false,"status":"pass"}'
else
    fail "Flaky rate detection"
fi

# ── Scenario 5: Full module validation ────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

if cargo test -p frankenterm-core --lib replay_shadow_rollout 2>&1 | grep -q "test result: ok"; then
    pass "All shadow rollout unit tests (35 tests)"
else
    fail "Shadow rollout unit tests"
fi

if cargo test -p frankenterm-core --test proptest_replay_shadow_rollout 2>&1 | grep -q "test result: ok"; then
    pass "All shadow rollout property tests (20 tests)"
else
    fail "Shadow rollout property tests"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"shadow_rollout\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
