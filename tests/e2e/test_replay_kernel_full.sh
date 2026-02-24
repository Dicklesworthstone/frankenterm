#!/usr/bin/env bash
# E2E smoke test: kernel determinism full integration suite (ft-og6q6.3.6)
#
# Validates cross-module kernel determinism: VirtualClock, ReplayScheduler,
# PaneMergeResolver, SideEffectBarrier, and ProvenanceEmitter working together.
#
# Summary JSON: {"test":"kernel_full","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Kernel Determinism Full Integration Suite ==="

# ── Scenario 1: Single-pane and multi-pane replay ─────────────────────────
echo ""
echo "--- Scenario 1: Replay Roundtrip ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration -- i01 i02 2>&1 | grep -q "test result: ok"; then
    pass "Single-pane and multi-pane replay roundtrip"
    echo '{"test":"kernel_full","scenario":1,"check":"replay_roundtrip","status":"pass"}'
else
    fail "Replay roundtrip"
fi

# ── Scenario 2: Checkpoint/resume and determinism ─────────────────────────
echo ""
echo "--- Scenario 2: Checkpoint and Determinism ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration -- i03 i14 2>&1 | grep -q "test result: ok"; then
    pass "Checkpoint/resume equivalence and deterministic decision IDs"
    echo '{"test":"kernel_full","scenario":2,"check":"checkpoint_determinism","status":"pass"}'
else
    fail "Checkpoint/determinism"
fi

# ── Scenario 3: Side-effect isolation ─────────────────────────────────────
echo ""
echo "--- Scenario 3: Side-Effect Isolation ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration -- i04 i11 i12 i19 2>&1 | grep -q "test result: ok"; then
    pass "Side-effect barrier: replay/live/counterfactual modes"
    echo '{"test":"kernel_full","scenario":3,"check":"side_effect_isolation","status":"pass"}'
else
    fail "Side-effect isolation"
fi

# ── Scenario 4: Clock and merge ───────────────────────────────────────────
echo ""
echo "--- Scenario 4: Clock and Merge ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration -- i05 i06 i07 i16 2>&1 | grep -q "test result: ok"; then
    pass "Clock anomaly, speed control, merge+schedule pipeline"
    echo '{"test":"kernel_full","scenario":4,"check":"clock_merge","status":"pass"}'
else
    fail "Clock and merge"
fi

# ── Scenario 5: Provenance and audit ──────────────────────────────────────
echo ""
echo "--- Scenario 5: Provenance and Audit ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration -- i08 i09 i10 i13 i17 i18 i22 i23 2>&1 | grep -q "test result: ok"; then
    pass "Provenance tracking, audit chain, JSONL roundtrip"
    echo '{"test":"kernel_full","scenario":5,"check":"provenance_audit","status":"pass"}'
else
    fail "Provenance and audit"
fi

# ── Scenario 6: Full integration suite ────────────────────────────────────
echo ""
echo "--- Scenario 6: Full Integration Suite ---"

if cargo test -p frankenterm-core --test replay_kernel_determinism_integration 2>&1 | grep -q "test result: ok"; then
    pass "All kernel determinism integration tests (24 tests)"
else
    fail "Full integration suite"
fi

# ── Scenario 7: Existing kernel unit tests ────────────────────────────────
echo ""
echo "--- Scenario 7: Kernel Unit Tests ---"

if cargo test -p frankenterm-core --lib recorder_replay::tests 2>&1 | grep -q "test result: ok"; then
    pass "All recorder_replay unit tests (84 tests)"
else
    fail "Kernel unit tests"
fi

# ── Scenario 8: Determinism property tests ────────────────────────────────
echo ""
echo "--- Scenario 8: Determinism Property Tests ---"

if cargo test -p frankenterm-core --test proptest_replay_determinism 2>&1 | grep -q "test result: ok"; then
    pass "All determinism property tests (19 tests)"
else
    fail "Determinism property tests"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"kernel_full\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
