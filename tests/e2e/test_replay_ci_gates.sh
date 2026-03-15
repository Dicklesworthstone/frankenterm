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

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_ci_gates-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_ci_gates_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_ci_gates_${RUN_ID}.smoke.log"

PASS_COUNT=0
FAIL_COUNT=0

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

log_json() {
    local scenario="$1" gate="$2" result="$3" evidence="$4" waiver="$5" status="$6"
    echo "{\"test\":\"ci_gates\",\"scenario\":${scenario},\"gate\":${gate},\"result\":\"${result}\",\"evidence_bundle\":${evidence},\"waiver_applied\":${waiver},\"status\":\"${status}\"}"
}

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

echo "=== Replay CI Gates E2E ==="

ensure_rch_ready

# ── Scenario 1: Gate 1 smoke pass ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Gate 1 Smoke Pass ---"

scenario1_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_ci_gate::tests::gate1_all_smoke_pass && grep -q "ok" "${scenario1_log}"; then
    pass "Gate 1 pass evaluation"
    log_json 1 1 "pass" false false "pass"
else
    fail "Gate 1 pass evaluation (see $(basename "${scenario1_log}"))"
    log_json 1 1 "fail" false false "fail"
fi

# ── Scenario 2: Gate 2 test failure blocks ─────────────────────────────
echo ""
echo "--- Scenario 2: Gate 2 Test Failure Blocks ---"

scenario2_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_ci_gate::tests::gate2_unit_test_failure && grep -q "ok" "${scenario2_log}"; then
    pass "Gate 2 failure detection"
    log_json 2 2 "fail" false false "pass"
else
    fail "Gate 2 failure detection (see $(basename "${scenario2_log}"))"
    log_json 2 2 "fail" false false "fail"
fi

# ── Scenario 3: Gate 3 regression with evidence ───────────────────────
echo ""
echo "--- Scenario 3: Gate 3 Regression with Evidence ---"

scenario3a_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario3a.log"
if run_rch_cargo_logged "${scenario3a_log}" test -p frankenterm-core --lib replay_ci_gate::tests::gate3_all_pass && grep -q "ok" "${scenario3a_log}"; then
    pass "Gate 3 pass with evidence bundle"
    log_json 3 3 "pass" true false "pass"
else
    fail "Gate 3 pass with evidence bundle (see $(basename "${scenario3a_log}"))"
    log_json 3 3 "pass" true false "fail"
fi

scenario3b_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario3b.log"
if run_rch_cargo_logged "${scenario3b_log}" test -p frankenterm-core --lib replay_ci_gate::tests::evidence_bundle_collects_artifact_paths && grep -q "ok" "${scenario3b_log}"; then
    pass "Evidence bundle artifact collection"
    log_json 3 3 "pass" true false "pass"
else
    fail "Evidence bundle artifact collection (see $(basename "${scenario3b_log}"))"
    log_json 3 3 "fail" true false "fail"
fi

# ── Scenario 4: Waiver bypasses check ─────────────────────────────────
echo ""
echo "--- Scenario 4: Waiver Bypass ---"

scenario4a_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario4a.log"
if run_rch_cargo_logged "${scenario4a_log}" test -p frankenterm-core --lib replay_ci_gate::tests::apply_waiver_changes_status && grep -q "ok" "${scenario4a_log}"; then
    pass "Waiver application"
    log_json 4 1 "pass" false true "pass"
else
    fail "Waiver application (see $(basename "${scenario4a_log}"))"
    log_json 4 1 "fail" false true "fail"
fi

scenario4b_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario4b.log"
if run_rch_cargo_logged "${scenario4b_log}" test -p frankenterm-core --lib replay_ci_gate::tests::apply_expired_waiver_no_change && grep -q "ok" "${scenario4b_log}"; then
    pass "Expired waiver rejected"
    log_json 4 1 "pass" false false "pass"
else
    fail "Expired waiver rejected (see $(basename "${scenario4b_log}"))"
    log_json 4 1 "fail" false false "fail"
fi

# ── Scenario 5: Full module passes ─────────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_ci_gates_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --lib replay_ci_gate && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All CI gate unit tests (56 tests)"
    log_json 5 0 "pass" false false "pass"
else
    fail "CI gate unit tests (see $(basename "${scenario5_log}"))"
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
