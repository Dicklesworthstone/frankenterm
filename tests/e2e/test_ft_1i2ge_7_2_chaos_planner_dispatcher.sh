#!/usr/bin/env bash
# E2E test for ft-1i2ge.7.2: Chaos/fault injection tests for planner+dispatcher
#
# Verifies that:
# 1. All 24 chaos tests pass (8 planner + 8 tx dispatcher + 8 idempotency)
# 2. Existing tx_e2e_scenario_matrix still passes (regression check)
# 3. Existing tx_correctness_suite still passes (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOG_DIR="${PROJECT_ROOT}/tests/e2e/logs"
mkdir -p "$LOG_DIR"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-chaos-planner-dispatcher-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/chaos_planner_dispatcher_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/chaos_planner_dispatcher_${RUN_ID}.smoke.log"

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

echo "=== ft-1i2ge.7.2 E2E: Chaos/Fault Injection for Planner+Dispatcher ==="
echo "Log directory: $LOG_DIR"
echo ""

ensure_rch_ready

# Step 1: Chaos planner+dispatcher tests
echo "[1/3] Running chaos planner+dispatcher tests (24 tests)..."
chaos_log="$LOG_DIR/chaos_tests_${RUN_ID}.log"
set +e
run_rch_cargo_logged "${chaos_log}" \
  test --package frankenterm-core \
  --test chaos_planner_dispatcher \
  --features subprocess-bridge \
  -- --nocapture
chaos_rc=$?
set -e
if [[ ${chaos_rc} -ne 0 ]]; then
  echo "FAIL: chaos planner+dispatcher tests failed (exit ${chaos_rc})" >&2
  echo "  See: ${chaos_log}"
  exit 1
fi
echo ""

# Step 2: Regression check against tx_e2e_scenario_matrix
echo "[2/3] Running tx E2E scenario matrix (regression check)..."
matrix_log="$LOG_DIR/scenario_matrix_${RUN_ID}.log"
set +e
run_rch_cargo_logged "${matrix_log}" \
  test --package frankenterm-core \
  --test tx_e2e_scenario_matrix \
  -- --nocapture
matrix_rc=$?
set -e
if [[ ${matrix_rc} -ne 0 ]]; then
  echo "FAIL: tx E2E scenario matrix regression failed (exit ${matrix_rc})" >&2
  echo "  See: ${matrix_log}"
  exit 1
fi
echo ""

# Step 3: Regression check against tx_correctness_suite
echo "[3/3] Running tx correctness suite (regression check)..."
correctness_log="$LOG_DIR/correctness_suite_${RUN_ID}.log"
set +e
run_rch_cargo_logged "${correctness_log}" \
  test --package frankenterm-core \
  --test tx_correctness_suite \
  -- --nocapture
correctness_rc=$?
set -e
if [[ ${correctness_rc} -ne 0 ]]; then
  echo "FAIL: tx correctness suite regression failed (exit ${correctness_rc})" >&2
  echo "  See: ${correctness_log}"
  exit 1
fi
echo ""

echo "=== All ft-1i2ge.7.2 tests passed ==="
echo "Logs: $LOG_DIR"
