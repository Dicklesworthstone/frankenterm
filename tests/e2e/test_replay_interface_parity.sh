#!/usr/bin/env bash
# E2E smoke test: replay interface parity (ft-og6q6.6.5)
#
# Validates that CLI, Robot Mode, and MCP tool schemas maintain
# consistent behavior and naming conventions.
#
# Summary JSON: {"test":"interface_parity","contract_pass":true,"smoke_pass":N,"status":"pass|fail"}

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="${REPO_ROOT}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-interface-parity-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_interface_parity_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_interface_parity_${RUN_ID}.smoke.log"
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

echo "=== Replay Interface Parity E2E ==="
ensure_rch_ready

# ── Scenario 1: Contract tests compile and pass ──────────────────────
echo ""
echo "--- Scenario 1: Interface Contract Tests ---"

scenario1_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario1.log"
if run_rch_cargo_logged "${scenario1_log}" test -p frankenterm-core --test replay_interface_contract \
    && grep -q "test result: ok" "${scenario1_log}"; then
    pass "Interface contract tests (42 tests)"
else
    fail "Interface contract tests (see $(basename "${scenario1_log}"))"
fi

# ── Scenario 2: Proptest suites pass ────────────────────────────────
echo ""
echo "--- Scenario 2: Property-Based Tests ---"

scenario2_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario2.log"
if run_rch_cargo_logged "${scenario2_log}" test -p frankenterm-core --test proptest_replay_mcp \
    && grep -q "test result: ok" "${scenario2_log}"; then
    pass "MCP property tests (15 tests)"
else
    fail "MCP property tests (see $(basename "${scenario2_log}"))"
fi

scenario3_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario3.log"
if run_rch_cargo_logged "${scenario3_log}" test -p frankenterm-core --test proptest_replay_robot \
    && grep -q "test result: ok" "${scenario3_log}"; then
    pass "Robot property tests (20 tests)"
else
    fail "Robot property tests (see $(basename "${scenario3_log}"))"
fi

# ── Scenario 3: Smoke tests (S-01..S-05) ───────────────────────────
echo ""
echo "--- Scenario 3: Smoke Tests ---"

# S-01: Exit code constants are defined
scenario4_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario4.log"
if run_rch_cargo_logged "${scenario4_log}" test -p frankenterm-core --test replay_interface_contract ic33_smoke_exit_code_pass \
    && grep -q "ok" "${scenario4_log}"; then
    pass "S-01: Exit code pass=0"
else
    fail "S-01: Exit code pass=0 (see $(basename "${scenario4_log}"))"
fi

# S-02: Default output mode
scenario5_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario5.log"
if run_rch_cargo_logged "${scenario5_log}" test -p frankenterm-core --test replay_interface_contract ic34_smoke_default_output_mode \
    && grep -q "ok" "${scenario5_log}"; then
    pass "S-02: Default output mode=Human"
else
    fail "S-02: Default output mode=Human (see $(basename "${scenario5_log}"))"
fi

# S-03: Minimal artifact inspect
scenario6_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario6.log"
if run_rch_cargo_logged "${scenario6_log}" test -p frankenterm-core --test replay_interface_contract ic35_smoke_inspect_minimal \
    && grep -q "ok" "${scenario6_log}"; then
    pass "S-03: Minimal artifact inspect"
else
    fail "S-03: Minimal artifact inspect (see $(basename "${scenario6_log}"))"
fi

# S-04: Identical diff produces zero divergences
scenario7_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario7.log"
if run_rch_cargo_logged "${scenario7_log}" test -p frankenterm-core --test replay_interface_contract ic36_smoke_diff_identical \
    && grep -q "ok" "${scenario7_log}"; then
    pass "S-04: Identical diff = zero divergences"
else
    fail "S-04: Identical diff = zero divergences (see $(basename "${scenario7_log}"))"
fi

# S-05: Empty artifact list is valid
scenario8_log="${LOG_DIR}/replay_interface_parity_${RUN_ID}.scenario8.log"
if run_rch_cargo_logged "${scenario8_log}" test -p frankenterm-core --test replay_interface_contract ic37_smoke_empty_artifact_list \
    && grep -q "ok" "${scenario8_log}"; then
    pass "S-05: Empty artifact list valid"
else
    fail "S-05: Empty artifact list valid (see $(basename "${scenario8_log}"))"
fi

# ── Summary ─────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
echo "{\"test\":\"interface_parity\",\"contract_pass\":$([ "$FAIL_COUNT" -eq 0 ] && echo true || echo false),\"smoke_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
