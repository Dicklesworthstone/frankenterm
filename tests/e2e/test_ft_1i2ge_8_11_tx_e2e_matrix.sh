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

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${SCRIPT_DIR}/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_8_11_tx_e2e_matrix" "${PROJECT_ROOT}"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for rch metadata artifacts" >&2
    exit 1
fi

command_requires_subprocess_bridge() {
  local previous=""
  local arg
  for arg in "$@"; do
    if [[ "${previous}" == "--features" && ",${arg}," == *",subprocess-bridge,"* ]]; then
      return 0
    fi
    previous="${arg}"
  done
  return 1
}

echo "=== ft-1i2ge.8.11 E2E: Tx Scenario Matrix ==="
echo "Log directory: $LOG_DIR"
echo ""

ensure_rch_ready

# Step 1: E2E scenario matrix
echo "[1/2] Running tx E2E scenario matrix (19 tests)..."
step1_log="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.scenario_matrix.log"
scenario_matrix_cmd=(
  cargo test --package frankenterm-core
  --features subprocess-bridge
  --test tx_e2e_scenario_matrix
  --
  --nocapture
)
if ! command_requires_subprocess_bridge "${scenario_matrix_cmd[@]}"; then
  echo "FAIL: tx_e2e_scenario_matrix command is missing subprocess-bridge" >&2
  exit 1
fi
run_rch_cargo_logged "${step1_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" "${scenario_matrix_cmd[@]}"
echo ""

# Step 2: Regression check against existing tx correctness suite
echo "[2/2] Running tx correctness suite (regression check)..."
step2_log="${LOG_DIR}/tx_e2e_matrix_${RUN_ID}.correctness_suite.log"
correctness_suite_cmd=(
  cargo test --package frankenterm-core
  --features subprocess-bridge
  --test tx_correctness_suite
  --
  --nocapture
)
if ! command_requires_subprocess_bridge "${correctness_suite_cmd[@]}"; then
  echo "FAIL: tx_correctness_suite command is missing subprocess-bridge" >&2
  exit 1
fi
run_rch_cargo_logged "${step2_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" "${correctness_suite_cmd[@]}"
echo ""

echo "=== All ft-1i2ge.8.11 tests passed ==="
echo "Logs: $LOG_DIR"
