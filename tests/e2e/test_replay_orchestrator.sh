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
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_orchestrator-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_orchestrator_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_orchestrator_${RUN_ID}.smoke.log"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift

    set +e
    (
        cd "${REPO_ROOT}"
        run_rch_cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi

    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi

    set +e
    run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
    local smoke_rc=$?
    set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

echo "=== Replay Test Orchestrator E2E ==="

ensure_rch_ready

# ── Scenario 1: Full test-all passes ──────────────────────────────────
echo ""
echo "--- Scenario 1: Full Orchestrator Pass ---"

scenario1_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_all_pass && grep -q "ok" "${scenario1_log}"; then
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
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_test_orchestrator::tests::orchestrate_gate1_fail_fast && grep -q "ok" "${scenario2_log}"; then
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
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --lib replay_test_orchestrator::tests::retention_prunes_old_files && grep -q "ok" "${scenario3_log}"; then
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
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --lib replay_test_orchestrator::tests::summary_markdown_contains_table && grep -q "ok" "${scenario4_log}"; then
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
if run_rch_cargo_logged "${scenario5a_log}" test -p frankenterm-core --lib replay_test_orchestrator && grep -q "test result: ok" "${scenario5a_log}"; then
    pass "All orchestrator unit tests (33 tests)"
    echo '{"test":"orchestrator","scenario":5,"gates_run":3,"gate_results":{"1":"pass","2":"pass","3":"pass"},"evidence_files":0,"status":"pass"}'
else
    fail "Orchestrator unit tests (see $(basename "${scenario5a_log}"))"
    echo '{"test":"orchestrator","scenario":5,"gates_run":0,"gate_results":{},"evidence_files":0,"status":"fail"}'
fi

scenario5b_log="${LOG_DIR}/replay_orchestrator_${RUN_ID}.scenario5b.log"
if run_rch_cargo_logged "${scenario5b_log}" test -p frankenterm-core --test proptest_replay_test_orchestrator && grep -q "test result: ok" "${scenario5b_log}"; then
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
