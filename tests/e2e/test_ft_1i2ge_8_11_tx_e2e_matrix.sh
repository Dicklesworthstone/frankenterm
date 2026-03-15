#!/usr/bin/env bash
# E2E test for ft-1i2ge.8.11: Deterministic E2E scenario matrix for tx run/rollback flows
#
# Verifies that:
# 1. All 19 scenario matrix tests pass (9 core scenarios + 10 cross-scenario checks)
# 2. Existing tx_correctness_suite still passes (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${PROJECT_ROOT}/tests/e2e/logs"
mkdir -p "$LOG_DIR"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-tx-e2e-matrix-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.smoke.log"

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
        cd "${PROJECT_ROOT}"
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

echo "=== ft-1i2ge.8.11 E2E: Tx Scenario Matrix ==="
echo "Log directory: $LOG_DIR"
echo ""

ensure_rch_ready

# Step 1: E2E scenario matrix
echo "[1/2] Running tx E2E scenario matrix (19 tests)..."
step1_log="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.scenario_matrix.log"
run_rch_cargo_logged "${step1_log}" test --package frankenterm-core \
  --test tx_e2e_scenario_matrix \
  -- --nocapture
echo ""

# Step 2: Regression check against existing tx correctness suite
echo "[2/2] Running tx correctness suite (regression check)..."
step2_log="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.correctness_suite.log"
run_rch_cargo_logged "${step2_log}" test --package frankenterm-core \
  --test tx_correctness_suite \
  -- --nocapture
echo ""

echo "=== All ft-1i2ge.8.11 tests passed ==="
echo "Logs: $LOG_DIR"
