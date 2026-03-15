#!/usr/bin/env bash
# E2E smoke test: replay merge / stable event ordering (ft-og6q6.3.2)
#
# Validates pane merge resolution, timestamp ordering, tie-breaking,
# and deterministic multi-pane interleaving using Rust tests as ground truth.
#
# Summary JSON: {"test":"replay_merge","scenario":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay_merge-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_merge_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_merge_${RUN_ID}.smoke.log"

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

echo "=== Replay Merge / Event Ordering E2E ==="

ensure_rch_ready

# ── Scenario 1: Basic merge ordering ──────────────────────────────────────
echo ""
echo "--- Scenario 1: Single-pane and multi-pane merge ---"

scenario1_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_merge::tests::single_pane_passthrough && grep -q "ok" "${scenario1_log}"; then
    pass "Single-pane passthrough"
    echo '{"test":"replay_merge","scenario":1,"check":"single_pane","status":"pass"}'
else
    fail "Single-pane passthrough (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Tie-breaking and determinism ──────────────────────────────
echo ""
echo "--- Scenario 2: Timestamp tie-breaking ---"

scenario2_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_merge::tests::same_timestamp_stable_order && grep -q "ok" "${scenario2_log}"; then
    pass "Same-timestamp stable ordering"
    echo '{"test":"replay_merge","scenario":2,"check":"tie_breaking","status":"pass"}'
else
    fail "Same-timestamp stable ordering (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Large-scale merge ─────────────────────────────────────────
echo ""
echo "--- Scenario 3: Large merge (100 panes) ---"

scenario3_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --lib replay_merge::tests::large_merge_100_panes && grep -q "ok" "${scenario3_log}"; then
    pass "Large merge 100 panes"
    echo '{"test":"replay_merge","scenario":3,"check":"large_merge","status":"pass"}'
else
    fail "Large merge 100 panes (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Full unit test suite ──────────────────────────────────────
echo ""
echo "--- Scenario 4: Full Unit Test Suite ---"

scenario4_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --lib replay_merge && grep -q "test result: ok" "${scenario4_log}"; then
    pass "All replay merge unit tests (27 tests)"
else
    fail "Replay merge unit tests (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Property tests ────────────────────────────────────────────
echo ""
echo "--- Scenario 5: Property Tests ---"

scenario5_log="${LOG_DIR}/replay_merge_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --test proptest_replay_merge && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All replay merge property tests (18 tests)"
else
    fail "Replay merge property tests (see $(basename "${scenario5_log}"))"
fi

# ── Summary ───────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"replay_merge\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
