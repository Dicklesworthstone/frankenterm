#!/usr/bin/env bash
# E2E smoke test: replay shadow rollout (ft-og6q6.7.5)
#
# Validates shadow mode, enforcement, kill switch, and flaky detection
# using the Rust module as ground truth.
#
# Summary JSON: {"test":"shadow_rollout","scenario":N,"mode":"shadow|enforce|killswitch",
#                "gate_result":"pass|fail","pr_blocked":true|false,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-shadow-rollout-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.smoke.log"
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
        fatal "rch is required for this replay e2e harness; refusing local cargo execution."
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

echo "=== Replay Shadow Rollout E2E ==="
ensure_rch_ready

# ── Scenario 1: Shadow mode, gate fails, PR not blocked ───────────────
echo ""
echo "--- Scenario 1: Shadow Mode ---"

scenario1_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --lib replay_shadow_rollout::tests::shadow_mode_fail_not_blocked \
    && grep -q "ok" "${scenario1_log}"; then
    pass "Shadow mode: fail not blocked"
    echo '{"test":"shadow_rollout","scenario":1,"mode":"shadow","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Shadow mode: fail not blocked (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Enforcement mode, gate fails, PR blocked ─────────────
echo ""
echo "--- Scenario 2: Enforcement Mode ---"

scenario2_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --lib replay_shadow_rollout::tests::enforce_mode_fail_blocked \
    && grep -q "ok" "${scenario2_log}"; then
    pass "Enforce mode: fail blocked"
    echo '{"test":"shadow_rollout","scenario":2,"mode":"enforce","gate_result":"fail","pr_blocked":true,"status":"pass"}'
else
    fail "Enforce mode: fail blocked (see $(basename "${scenario2_log}"))"
fi

# ── Scenario 3: Kill switch, no enforcement ───────────────────────────
echo ""
echo "--- Scenario 3: Kill Switch ---"

scenario3_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --lib replay_shadow_rollout::tests::kill_switch_overrides_enforcement \
    && grep -q "ok" "${scenario3_log}"; then
    pass "Kill switch overrides"
    echo '{"test":"shadow_rollout","scenario":3,"mode":"killswitch","gate_result":"fail","pr_blocked":false,"status":"pass"}'
else
    fail "Kill switch overrides (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 4: Flaky test detection ──────────────────────────────────
echo ""
echo "--- Scenario 4: Flaky Detection ---"

scenario4_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --lib replay_shadow_rollout::tests::flaky_rate_critical \
    && grep -q "ok" "${scenario4_log}"; then
    pass "Flaky rate detection"
    echo '{"test":"shadow_rollout","scenario":4,"mode":"shadow","gate_result":"flaky","pr_blocked":false,"status":"pass"}'
else
    fail "Flaky rate detection (see $(basename "${scenario4_log}"))"
fi

# ── Scenario 5: Full module validation ────────────────────────────────
echo ""
echo "--- Scenario 5: Full Module Validation ---"

scenario5_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --lib replay_shadow_rollout \
    && grep -q "test result: ok" "${scenario5_log}"; then
    pass "All shadow rollout unit tests (35 tests)"
else
    fail "Shadow rollout unit tests (see $(basename "${scenario5_log}"))"
fi

scenario6_log="${LOG_DIR}/replay_shadow_rollout_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" test -p frankenterm-core --test proptest_replay_shadow_rollout \
    && grep -q "test result: ok" "${scenario6_log}"; then
    pass "All shadow rollout property tests (20 tests)"
else
    fail "Shadow rollout property tests (see $(basename "${scenario6_log}"))"
fi

# ── Summary ─────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"shadow_rollout\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
