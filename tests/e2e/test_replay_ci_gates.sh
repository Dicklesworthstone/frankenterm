#!/usr/bin/env bash
# E2E smoke test: replay CI gates (ft-og6q6.7.4)
#
# Validates Gate 1/2/3 evaluation logic, waiver parsing, and evidence
# bundle generation using the Rust module as ground truth.
#
# Summary JSON: {"test":"ci_gates","scenario":N,"gate":1|2|3,"result":"pass|fail",
#                "evidence_bundle":true|false,"waiver_applied":true|false,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

log_json() {
    local scenario="$1" gate="$2" result="$3" evidence="$4" waiver="$5" status="$6"
    echo "{\"test\":\"ci_gates\",\"scenario\":${scenario},\"gate\":${gate},\"result\":\"${result}\",\"evidence_bundle\":${evidence},\"waiver_applied\":${waiver},\"status\":\"${status}\"}"
}

echo "=== Replay CI Gates E2E ==="

# ── Scenario 1: Gate 1 smoke pass ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Gate 1 Smoke Pass ---"

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::gate1_all_smoke_pass 2>&1 | grep -q "ok"; then
    pass "Gate 1 pass evaluation"
    log_json 1 1 "pass" false false "pass"
else
    fail "Gate 1 pass evaluation"
    log_json 1 1 "fail" false false "fail"
fi

# ── Scenario 2: Gate 2 test failure blocks ─────────────────────────────
echo ""
echo "--- Scenario 2: Gate 2 Test Failure Blocks ---"

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::gate2_unit_test_failure 2>&1 | grep -q "ok"; then
    pass "Gate 2 failure detection"
    log_json 2 2 "fail" false false "pass"
else
    fail "Gate 2 failure detection"
    log_json 2 2 "fail" false false "fail"
fi

# ── Scenario 3: Gate 3 regression with evidence ───────────────────────
echo ""
echo "--- Scenario 3: Gate 3 Regression with Evidence ---"

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::gate3_all_pass 2>&1 | grep -q "ok"; then
    pass "Gate 3 pass with evidence bundle"
    log_json 3 3 "pass" true false "pass"
else
    fail "Gate 3 pass with evidence bundle"
    log_json 3 3 "pass" true false "fail"
fi

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::evidence_bundle_collects_artifact_paths 2>&1 | grep -q "ok"; then
    pass "Evidence bundle artifact collection"
    log_json 3 3 "pass" true false "pass"
else
    fail "Evidence bundle artifact collection"
    log_json 3 3 "fail" true false "fail"
fi

# ── Scenario 4: Waiver bypasses check ─────────────────────────────────
echo ""
echo "--- Scenario 4: Waiver Bypass ---"

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::apply_waiver_changes_status 2>&1 | grep -q "ok"; then
    pass "Waiver application"
    log_json 4 1 "pass" false true "pass"
else
    fail "Waiver application"
    log_json 4 1 "fail" false true "fail"
fi

if cargo test -p frankenterm-core --lib replay_ci_gate::tests::apply_expired_waiver_no_change 2>&1 | grep -q "ok"; then
    pass "Expired waiver rejected"
    log_json 4 1 "pass" false false "pass"
else
    fail "Expired waiver rejected"
    log_json 4 1 "fail" false false "fail"
fi

# ── Scenario 5: Full module passes ─────────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

if cargo test -p frankenterm-core --lib replay_ci_gate 2>&1 | grep -q "test result: ok"; then
    pass "All CI gate unit tests (56 tests)"
    log_json 5 0 "pass" false false "pass"
else
    fail "CI gate unit tests"
    log_json 5 0 "fail" false false "fail"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"ci_gates\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
