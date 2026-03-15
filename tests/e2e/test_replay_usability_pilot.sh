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
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-usability-pilot-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.smoke.log"

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

echo "=== Replay Usability Pilot E2E ==="
ensure_rch_ready

# ── Scenario 1: Pilot scenarios and metrics ─────────────────────────────
echo ""
echo "--- Scenario 1: Pilot Scenario Enum ---"

scenario1_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_usability_pilot::tests::scenario_str_roundtrip \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Scenario enum roundtrip"
    echo '{"test":"usability_pilot","scenario":1,"status":"pass"}'
else
    fail "Scenario enum roundtrip"
fi

# ── Scenario 2: Feedback log and metrics ────────────────────────────────
echo ""
echo "--- Scenario 2: Feedback Log Metrics ---"

scenario2_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_usability_pilot::tests::metrics_calculation \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Metrics calculation"
    echo '{"test":"usability_pilot","scenario":2,"status":"pass"}'
else
    fail "Metrics calculation"
fi

# ── Scenario 3: Pilot evaluation ────────────────────────────────────────
echo ""
echo "--- Scenario 3: Pilot Evaluation ---"

scenario3_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --lib replay_usability_pilot::tests::evaluation_passes_default_criteria \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Evaluation passes default criteria"
    echo '{"test":"usability_pilot","scenario":3,"status":"pass"}'
else
    fail "Evaluation passes default criteria"
fi

# ── Scenario 4: Improvement extraction ──────────────────────────────────
echo ""
echo "--- Scenario 4: Improvement Extraction ---"

scenario4_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --lib replay_usability_pilot::tests::extract_improvements_from_log \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Improvement extraction"
    echo '{"test":"usability_pilot","scenario":4,"status":"pass"}'
else
    fail "Improvement extraction"
fi

# ── Scenario 5: Full module validation ──────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --lib replay_usability_pilot \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All usability pilot unit tests"
else
    fail "Usability pilot unit tests"
fi

scenario5b_log="${LOG_DIR}/replay_usability_pilot_${RUN_ID}.scenario5b.log"
if run_rch_cargo_logged "${scenario5b_log}" test -p frankenterm-core --test proptest_replay_usability_pilot \
    && grep -q "test result: ok" "${scenario5b_log}"; then
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
