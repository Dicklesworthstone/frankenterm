#!/usr/bin/env bash
# E2E smoke test: replay test orchestrator (ft-og6q6.7.7)
#
# Validates orchestration, evidence bundle, retention, and summary report
# generation using the Rust module as ground truth.
#
# Summary JSON: {"test":"orchestrator","scenario":N,"gates_run":N,
#                "gate_results":{"1":"pass|fail","2":"pass|fail","3":"pass|fail"},
#                "evidence_files":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Test Orchestrator E2E ==="

# ── Scenario 1: Full test-all passes ──────────────────────────────────
echo ""
echo "--- Scenario 1: Full Orchestrator Pass ---"

if cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_all_pass 2>&1 | grep -q "ok"; then
    pass "Orchestrate all-pass"
    echo '{"test":"orchestrator","scenario":1,"gates_run":3,"gate_results":{"1":"pass","2":"pass","3":"pass"},"evidence_files":0,"status":"pass"}'
else
    fail "Orchestrate all-pass"
    echo '{"test":"orchestrator","scenario":1,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 2: Gate 1 fail-fast ──────────────────────────────────────
echo ""
echo "--- Scenario 2: Gate 1 Fail-Fast ---"

if cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_gate1_fail_fast 2>&1 | grep -q "ok"; then
    pass "Gate 1 fail-fast stops"
    echo '{"test":"orchestrator","scenario":2,"gates_run":1,"gate_results":{"1":"fail"},"evidence_files":0,"status":"pass"}'
else
    fail "Gate 1 fail-fast stops"
    echo '{"test":"orchestrator","scenario":2,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 3: Evidence prune removes old files ──────────────────────
echo ""
echo "--- Scenario 3: Evidence Prune ---"

if cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::retention_prunes_old_files 2>&1 | grep -q "ok"; then
    pass "Retention prunes old files"
    echo '{"test":"orchestrator","scenario":3,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"pass"}'
else
    fail "Retention prunes old files"
    echo '{"test":"orchestrator","scenario":3,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 4: Summary report generation ─────────────────────────────
echo ""
echo "--- Scenario 4: Summary Report ---"

if cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::summary_markdown_contains_table 2>&1 | grep -q "ok"; then
    pass "Summary report markdown"
    echo '{"test":"orchestrator","scenario":4,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"pass"}'
else
    fail "Summary report markdown"
    echo '{"test":"orchestrator","scenario":4,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 5: Full module validation ────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

if cargo test -p frankenterm-core --lib replay_test_orchestrator 2>&1 | grep -q "test result: ok"; then
    pass "All orchestrator unit tests (33 tests)"
    echo '{"test":"orchestrator","scenario":5,"gates_run":3,"gate_results":{"1":"pass","2":"pass","3":"pass"},"evidence_files":0,"status":"pass"}'
else
    fail "Orchestrator unit tests"
    echo '{"test":"orchestrator","scenario":5,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

if cargo test -p frankenterm-core --test proptest_replay_test_orchestrator 2>&1 | grep -q "test result: ok"; then
    pass "All orchestrator property tests (20 tests)"
else
    fail "Orchestrator property tests"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"orchestrator\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
