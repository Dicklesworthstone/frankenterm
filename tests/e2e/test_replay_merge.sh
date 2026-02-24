#!/usr/bin/env bash
# E2E smoke test: replay merge / stable event ordering (ft-og6q6.3.2)
#
# Validates pane merge resolution, timestamp ordering, tie-breaking,
# and deterministic multi-pane interleaving using Rust tests as ground truth.
#
# Summary JSON: {"test":"replay_merge","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Merge / Event Ordering E2E ==="

# ── Scenario 1: Basic merge ordering ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Single-pane and multi-pane merge ---"

if cargo test -p frankenterm-core --lib replay_merge::tests::single_pane_passthrough 2>&1 | grep -q "ok"; then
    pass "Single-pane passthrough"
    echo '{"test":"replay_merge","scenario":1,"check":"single_pane","status":"pass"}'
else
    fail "Single-pane passthrough"
fi

# ── Scenario 2: Tie-breaking and determinism ──────────────────────────────
echo ""
echo "--- Scenario 2: Timestamp tie-breaking ---"

if cargo test -p frankenterm-core --lib replay_merge::tests::same_timestamp_stable_order 2>&1 | grep -q "ok"; then
    pass "Same-timestamp stable ordering"
    echo '{"test":"replay_merge","scenario":2,"check":"tie_breaking","status":"pass"}'
else
    fail "Same-timestamp stable ordering"
fi

# ── Scenario 3: Large-scale merge ─────────────────────────────────────────
echo ""
echo "--- Scenario 3: Large merge (100 panes) ---"

if cargo test -p frankenterm-core --lib replay_merge::tests::large_merge_100_panes 2>&1 | grep -q "ok"; then
    pass "Large merge 100 panes"
    echo '{"test":"replay_merge","scenario":3,"check":"large_merge","status":"pass"}'
else
    fail "Large merge 100 panes"
fi

# ── Scenario 4: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 4: Full Unit Test Suite ---"

if cargo test -p frankenterm-core --lib replay_merge 2>&1 | grep -q "test result: ok"; then
    pass "All replay merge unit tests (27 tests)"
else
    fail "Replay merge unit tests"
fi

# ── Scenario 5: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Property Tests ---"

if cargo test -p frankenterm-core --test proptest_replay_merge 2>&1 | grep -q "test result: ok"; then
    pass "All replay merge property tests (18 tests)"
else
    fail "Replay merge property tests"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"replay_merge\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
