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

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
GUARD_LIB="${REPO_ROOT}/tests/e2e/lib_rch_guards.sh"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_orchestrator-${RUN_ID}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "replay_orchestrator" "${REPO_ROOT}"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

echo "=== Replay Test Orchestrator E2E ==="

ensure_rch_ready

# ── Scenario 1: Full test-all passes ──────────────────────────────────
echo ""
echo "--- Scenario 1: Full Orchestrator Pass ---"

scenario1_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_all_pass && grep -q "ok" "${scenario1_log}"; then
    pass "Orchestrate all-pass"
    echo '{"test":"orchestrator","scenario":1,"gates_run":3,"gate_results":{"1":"pass","2":"pass","3":"pass"},"evidence_files":0,"status":"pass"}'
else
    fail "Orchestrate all-pass (see $(basename "${scenario1_log}"))"
    echo '{"test":"orchestrator","scenario":1,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 2: Gate 1 fail-fast ──────────────────────────────────────
echo ""
echo "--- Scenario 2: Gate 1 Fail-Fast ---"

scenario2_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_gate1_fail_fast && grep -q "ok" "${scenario2_log}"; then
    pass "Gate 1 fail-fast stops"
    echo '{"test":"orchestrator","scenario":2,"gates_run":1,"gate_results":{"1":"fail"},"evidence_files":0,"status":"pass"}'
else
    fail "Gate 1 fail-fast stops (see $(basename "${scenario2_log}"))"
    echo '{"test":"orchestrator","scenario":2,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 3: Evidence prune removes old files ──────────────────────
echo ""
echo "--- Scenario 3: Evidence Prune ---"

scenario3_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::retention_prunes_old_files && grep -q "ok" "${scenario3_log}"; then
    pass "Retention prunes old files"
    echo '{"test":"orchestrator","scenario":3,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"pass"}'
else
    fail "Retention prunes old files (see $(basename "${scenario3_log}"))"
    echo '{"test":"orchestrator","scenario":3,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 4: Summary report generation ─────────────────────────────
echo ""
echo "--- Scenario 4: Summary Report ---"

scenario4_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_test_orchestrator::tests::summary_markdown_contains_table && grep -q "ok" "${scenario4_log}"; then
    pass "Summary report markdown"
    echo '{"test":"orchestrator","scenario":4,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"pass"}'
else
    fail "Summary report markdown (see $(basename "${scenario4_log}"))"
    echo '{"test":"orchestrator","scenario":4,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

# ── Scenario 5: Full module validation ────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5a_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario5a.log"
if run_rch_cargo_logged "${scenario5a_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib replay_test_orchestrator && grep -q "test result: ok" "${scenario5a_log}"; then
    pass "All orchestrator unit tests (33 tests)"
    echo '{"test":"orchestrator","scenario":5,"gates_run":3,"gate_results":{"1":"pass","2":"pass","3":"pass"},"evidence_files":0,"status":"pass"}'
else
    fail "Orchestrator unit tests (see $(basename "${scenario5a_log}"))"
    echo '{"test":"orchestrator","scenario":5,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

scenario5b_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario5b.log"
if run_rch_cargo_logged "${scenario5b_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test proptest_replay_test_orchestrator && grep -q "test result: ok" "${scenario5b_log}"; then
    pass "All orchestrator property tests (20 tests)"
else
    fail "Orchestrator property tests (see $(basename "${scenario5b_log}"))"
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
