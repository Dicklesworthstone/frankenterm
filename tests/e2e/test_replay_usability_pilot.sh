#!/usr/bin/env bash
# E2E smoke test: replay usability pilot (ft-og6q6.7.8)
#
# Validates pilot framework, feedback log, metrics, evaluation,
# and improvement extraction using the Rust module as ground truth.
#
# Summary JSON: {"test":"usability_pilot","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Usability Pilot E2E ==="

# ── Scenario 1: Pilot scenarios and metrics ─────────────────────────────
echo ""
echo "--- Scenario 1: Pilot Scenario Enum ---"

if cargo test -p frankenterm-core --lib replay_usability_pilot::tests::scenario_str_roundtrip 2>&1 | grep -q "ok"; then
    pass "Scenario enum roundtrip"
    echo '{"test":"usability_pilot","scenario":1,"status":"pass"}'
else
    fail "Scenario enum roundtrip"
fi

# ── Scenario 2: Feedback log and metrics ────────────────────────────────
echo ""
echo "--- Scenario 2: Feedback Log Metrics ---"

if cargo test -p frankenterm-core --lib replay_usability_pilot::tests::metrics_calculation 2>&1 | grep -q "ok"; then
    pass "Metrics calculation"
    echo '{"test":"usability_pilot","scenario":2,"status":"pass"}'
else
    fail "Metrics calculation"
fi

# ── Scenario 3: Pilot evaluation ────────────────────────────────────────
echo ""
echo "--- Scenario 3: Pilot Evaluation ---"

if cargo test -p frankenterm-core --lib replay_usability_pilot::tests::evaluation_passes_default_criteria 2>&1 | grep -q "ok"; then
    pass "Evaluation passes default criteria"
    echo '{"test":"usability_pilot","scenario":3,"status":"pass"}'
else
    fail "Evaluation passes default criteria"
fi

# ── Scenario 4: Improvement extraction ──────────────────────────────────
echo ""
echo "--- Scenario 4: Improvement Extraction ---"

if cargo test -p frankenterm-core --lib replay_usability_pilot::tests::extract_improvements_from_log 2>&1 | grep -q "ok"; then
    pass "Improvement extraction"
    echo '{"test":"usability_pilot","scenario":4,"status":"pass"}'
else
    fail "Improvement extraction"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

if cargo test -p frankenterm-core --lib replay_usability_pilot 2>&1 | grep -q "test result: ok"; then
    pass "All usability pilot unit tests"
else
    fail "Usability pilot unit tests"
fi

if cargo test -p frankenterm-core --test proptest_replay_usability_pilot 2>&1 | grep -q "test result: ok"; then
    pass "All usability pilot property tests (20 tests)"
else
    fail "Usability pilot property tests"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"usability_pilot\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
