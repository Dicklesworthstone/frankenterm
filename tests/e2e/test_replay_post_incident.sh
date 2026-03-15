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
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-post-incident-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_post_incident_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_post_incident_${RUN_ID}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""
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

resolve_timeout_bin() {
    if command -v timeout >/dev/null 2>&1; then
        TIMEOUT_BIN="timeout"
    elif command -v gtimeout >/dev/null 2>&1; then
        TIMEOUT_BIN="gtimeout"
    else
        TIMEOUT_BIN=""
    fi
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
        env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
            rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log="${output_file%.log}.rch_queue_timeout.log"
        if ! run_rch queue >"${queue_log}" 2>&1; then
            queue_log="${output_file}"
        fi
        fatal "rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s; refusing stalled remote execution. See ${queue_log}"
    fi
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this replay e2e harness; refusing local cargo execution."
    fi
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
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

echo "=== Replay Post-Incident Feedback Loop E2E ==="
ensure_rch_ready

# ── Scenario 1: Pipeline execution ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Pipeline Execution ---"

scenario1_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_post_incident::tests::pipeline_success \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Pipeline execution succeeds"
    echo '{"test":"post_incident","scenario":1,"status":"pass"}'
else
    fail "Pipeline execution (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Input validation ────────────────────────────────────────
echo ""
echo "--- Scenario 2: Input Validation ---"

scenario2_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_post_incident::tests::empty_incident_id_error \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Empty incident_id rejected"
    echo '{"test":"post_incident","scenario":2,"status":"pass"}'
else
    fail "Empty incident_id rejected (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Incident corpus coverage ────────────────────────────────
echo ""
echo "--- Scenario 3: Incident Corpus ---"

scenario3_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --lib replay_post_incident::tests::corpus_gap_detection \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Gap detection works"
    echo '{"test":"post_incident","scenario":3,"status":"pass"}'
else
    fail "Gap detection (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Coverage report ─────────────────────────────────────────
echo ""
echo "--- Scenario 4: Coverage Report ---"

scenario4_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --lib replay_post_incident::tests::coverage_report_counts \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Coverage report counts"
    echo '{"test":"post_incident","scenario":4,"status":"pass"}'
else
    fail "Coverage report counts (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --lib replay_post_incident \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All post-incident unit tests"
else
    fail "Post-incident unit tests (see $(basename "${scenario5_log}"))"
fi

scenario6_log="${LOG_DIR}/replay_post_incident_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" test -p frankenterm-core --test proptest_replay_post_incident \
    && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All post-incident property tests (20 tests)"
else
    fail "Post-incident property tests (see $(basename "${scenario6_log}"))"
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
