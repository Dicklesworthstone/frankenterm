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

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${SCRIPT_DIR}/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_7_2_chaos_planner_dispatcher" "${PROJECT_ROOT}"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for rch metadata artifacts" >&2
    exit 1
fi

echo "=== ft-1i2ge.7.2 E2E: Chaos/Fault Injection for Planner+Dispatcher ==="
echo "Log directory: $LOG_DIR"
echo ""

ensure_rch_ready

# Step 1: Chaos planner+dispatcher tests
echo "[1/3] Running chaos planner+dispatcher tests (24 tests)..."
chaos_log="$LOG_DIR/chaos_tests_${RUN_ID}.log"
set +e
run_rch_cargo_logged "${chaos_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo \
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
run_rch_cargo_logged "${matrix_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo \
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
run_rch_cargo_logged "${correctness_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo \
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
