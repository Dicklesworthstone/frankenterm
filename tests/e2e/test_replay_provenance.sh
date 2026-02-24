#!/usr/bin/env bash
# E2E smoke test: replay provenance logs and decision trace (ft-og6q6.3.4)
#
# Validates provenance chain integrity, decision trace emission,
# verbosity levels, and trace serde roundtrips using Rust tests as ground truth.
#
# Summary JSON: {"test":"replay_provenance","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Provenance Logs E2E ==="

# ── Scenario 1: Chain integrity ───────────────────────────────────────────
echo ""
echo "--- Scenario 1: Provenance Chain Integrity ---"

if cargo test -p frankenterm-core --lib -- replay_provenance::tests::chain 2>&1 | grep -q "ok"; then
    pass "Provenance chain integrity"
    echo '{"test":"replay_provenance","scenario":1,"check":"chain_integrity","status":"pass"}'
else
    fail "Provenance chain integrity"
fi

# ── Scenario 2: Verbosity levels ──────────────────────────────────────────
echo ""
echo "--- Scenario 2: Verbosity Levels ---"

if cargo test -p frankenterm-core --lib -- replay_provenance::tests::verbosity 2>&1 | grep -q "ok"; then
    pass "Verbosity level control"
    echo '{"test":"replay_provenance","scenario":2,"check":"verbosity","status":"pass"}'
else
    fail "Verbosity level control"
fi

# ── Scenario 3: Trace serde roundtrip ─────────────────────────────────────
echo ""
echo "--- Scenario 3: Trace Serde Roundtrip ---"

if cargo test -p frankenterm-core --lib -- replay_provenance::tests::trace_serde_roundtrip 2>&1 | grep -q "ok"; then
    pass "Trace serde roundtrip"
    echo '{"test":"replay_provenance","scenario":3,"check":"serde","status":"pass"}'
else
    fail "Trace serde roundtrip"
fi

# ── Scenario 4: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 4: Full Unit Test Suite ---"

if cargo test -p frankenterm-core --lib replay_provenance 2>&1 | grep -q "test result: ok"; then
    pass "All provenance unit tests (34 tests)"
else
    fail "Provenance unit tests"
fi

# ── Scenario 5: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Property Tests ---"

if cargo test -p frankenterm-core --test proptest_replay_provenance 2>&1 | grep -q "test result: ok"; then
    pass "All provenance property tests (23 tests)"
else
    fail "Provenance property tests"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"replay_provenance\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
