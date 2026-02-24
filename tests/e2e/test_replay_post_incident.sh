#!/usr/bin/env bash
# E2E smoke test: replay post-incident feedback loop (ft-og6q6.7.6)
#
# Validates pipeline execution, input validation, incident corpus,
# and coverage tracking using the Rust module as ground truth.
#
# Summary JSON: {"test":"post_incident","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Post-Incident Feedback Loop E2E ==="

# ── Scenario 1: Pipeline execution ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Pipeline Execution ---"

if cargo test -p frankenterm-core --lib replay_post_incident::tests::pipeline_success 2>&1 | grep -q "ok"; then
    pass "Pipeline execution succeeds"
    echo '{"test":"post_incident","scenario":1,"status":"pass"}'
else
    fail "Pipeline execution"
fi

# ── Scenario 2: Input validation ────────────────────────────────────────
echo ""
echo "--- Scenario 2: Input Validation ---"

if cargo test -p frankenterm-core --lib replay_post_incident::tests::empty_incident_id_error 2>&1 | grep -q "ok"; then
    pass "Empty incident_id rejected"
    echo '{"test":"post_incident","scenario":2,"status":"pass"}'
else
    fail "Empty incident_id rejected"
fi

# ── Scenario 3: Incident corpus coverage ────────────────────────────────
echo ""
echo "--- Scenario 3: Incident Corpus ---"

if cargo test -p frankenterm-core --lib replay_post_incident::tests::corpus_gap_detection 2>&1 | grep -q "ok"; then
    pass "Gap detection works"
    echo '{"test":"post_incident","scenario":3,"status":"pass"}'
else
    fail "Gap detection"
fi

# ── Scenario 4: Coverage report ─────────────────────────────────────────
echo ""
echo "--- Scenario 4: Coverage Report ---"

if cargo test -p frankenterm-core --lib replay_post_incident::tests::coverage_report_counts 2>&1 | grep -q "ok"; then
    pass "Coverage report counts"
    echo '{"test":"post_incident","scenario":4,"status":"pass"}'
else
    fail "Coverage report counts"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

if cargo test -p frankenterm-core --lib replay_post_incident 2>&1 | grep -q "test result: ok"; then
    pass "All post-incident unit tests"
else
    fail "Post-incident unit tests"
fi

if cargo test -p frankenterm-core --test proptest_replay_post_incident 2>&1 | grep -q "test result: ok"; then
    pass "All post-incident property tests (20 tests)"
else
    fail "Post-incident property tests"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"post_incident\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
