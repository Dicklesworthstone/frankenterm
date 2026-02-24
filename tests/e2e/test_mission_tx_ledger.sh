#!/usr/bin/env bash
# E2E smoke test: Intent Ledger and Causal Receipt Persistence (ft-1i2ge.8.2)
#
# Validates ledger hash chain integrity, serde roundtrips, query surfaces,
# recorder lifecycle, and tamper detection using Rust tests as ground truth.
#
# Summary JSON: {"test":"mission_tx_ledger","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Intent Ledger E2E Suite ==="

# ── Scenario 1: Ledger creation and hash chain ──────────────────────────
echo ""
echo "--- Scenario 1: Hash Chain Integrity ---"

if cargo test -p frankenterm-core --lib -- plan::tests::ledger_hash_chain plan::tests::ledger_validate_happy plan::tests::ledger_large_chain 2>&1 | grep -q "test result: ok"; then
    pass "Hash chain integrity, validation, and large chain"
    echo '{"test":"mission_tx_ledger","scenario":1,"check":"hash_chain","status":"pass"}'
else
    fail "Hash chain integrity"
fi

# ── Scenario 2: Tamper detection ─────────────────────────────────────────
echo ""
echo "--- Scenario 2: Tamper Detection ---"

if cargo test -p frankenterm-core --lib -- plan::tests::ledger_validate_detects 2>&1 | grep -q "test result: ok"; then
    pass "Tamper detection: broken chain, invalid genesis, tx mismatch"
    echo '{"test":"mission_tx_ledger","scenario":2,"check":"tamper_detection","status":"pass"}'
else
    fail "Tamper detection"
fi

# ── Scenario 3: Serde roundtrips ─────────────────────────────────────────
echo ""
echo "--- Scenario 3: Serde Roundtrips ---"

if cargo test -p frankenterm-core --lib -- plan::tests::ledger_jsonl_roundtrip plan::tests::ledger_serde_roundtrip plan::tests::ledger_entry_serde plan::tests::ledger_correlation_serde plan::tests::ledger_all_entry_kinds 2>&1 | grep -q "test result: ok"; then
    pass "JSONL, JSON, entry, correlation, and entry-kind serde roundtrips"
    echo '{"test":"mission_tx_ledger","scenario":3,"check":"serde","status":"pass"}'
else
    fail "Serde roundtrips"
fi

# ── Scenario 4: Query surfaces ───────────────────────────────────────────
echo ""
echo "--- Scenario 4: Query Surfaces ---"

if cargo test -p frankenterm-core --lib -- plan::tests::ledger_entries_of_kind plan::tests::ledger_entries_in_range plan::tests::ledger_entries_for_pane plan::tests::ledger_entries_for_agent plan::tests::ledger_current_state plan::tests::ledger_state_timeline 2>&1 | grep -q "test result: ok"; then
    pass "Kind, range, pane, agent, state, and timeline queries"
    echo '{"test":"mission_tx_ledger","scenario":4,"check":"queries","status":"pass"}'
else
    fail "Query surfaces"
fi

# ── Scenario 5: Recorder lifecycle ───────────────────────────────────────
echo ""
echo "--- Scenario 5: Recorder Lifecycle ---"

if cargo test -p frankenterm-core --lib -- plan::tests::ledger_recorder 2>&1 | grep -q "test result: ok"; then
    pass "Recorder happy path and compensation path"
    echo '{"test":"mission_tx_ledger","scenario":5,"check":"recorder","status":"pass"}'
else
    fail "Recorder lifecycle"
fi

# ── Scenario 6: Full unit test suite ─────────────────────────────────────
echo ""
echo "--- Scenario 6: Full Unit Test Suite ---"

if cargo test -p frankenterm-core --lib 'plan::tests::ledger' 2>&1 | grep -q "test result: ok"; then
    pass "All intent ledger unit tests (30 tests)"
else
    fail "Full unit test suite"
fi

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"mission_tx_ledger\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
