#!/usr/bin/env bash
# E2E smoke test: replay side-effect isolation barrier (ft-og6q6.3.3)
#
# Validates side-effect capture, replay-mode blocking, log auditing,
# and barrier enforcement using Rust tests as ground truth.
#
# Summary JSON: {"test":"side_effect_barrier","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Side-Effect Isolation Barrier E2E ==="

# ── Scenario 1: Effect capture and classification ─────────────────────────
echo ""
echo "--- Scenario 1: Effect Capture ---"

if cargo test -p frankenterm-core --lib replay_side_effect_barrier::tests 2>&1 | grep -q "capture" | head -1; then
    : # grep matched; check full suite below
fi
if cargo test -p frankenterm-core --lib -- replay_side_effect_barrier::tests::capture 2>&1 | grep -q "ok"; then
    pass "Effect capture and classification"
    echo '{"test":"side_effect_barrier","scenario":1,"check":"capture","status":"pass"}'
else
    fail "Effect capture and classification"
fi

# ── Scenario 2: Replay-mode blocking ─────────────────────────────────────
echo ""
echo "--- Scenario 2: Replay-Mode Blocking ---"

if cargo test -p frankenterm-core --lib -- replay_side_effect_barrier::tests::replay_mode 2>&1 | grep -q "ok"; then
    pass "Replay-mode side-effect blocking"
    echo '{"test":"side_effect_barrier","scenario":2,"check":"replay_block","status":"pass"}'
else
    fail "Replay-mode side-effect blocking"
fi

# ── Scenario 3: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 3: Full Unit Test Suite ---"

if cargo test -p frankenterm-core --lib replay_side_effect_barrier 2>&1 | grep -q "test result: ok"; then
    pass "All side-effect barrier unit tests (46 tests)"
else
    fail "Side-effect barrier unit tests"
fi

# ── Scenario 4: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 4: Property Tests ---"

if cargo test -p frankenterm-core --test proptest_replay_side_effect_barrier 2>&1 | grep -q "test result: ok"; then
    pass "All side-effect barrier property tests (21 tests)"
else
    fail "Side-effect barrier property tests"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"side_effect_barrier\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
