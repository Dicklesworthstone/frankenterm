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
cd "$PROJECT_ROOT"

LOG_DIR="${TMPDIR:-/tmp}/ft_1i2ge_G2_logs"
mkdir -p "$LOG_DIR"

echo "=== ft-1i2ge.7.2 E2E: Chaos/Fault Injection for Planner+Dispatcher ==="
echo "Log directory: $LOG_DIR"
echo ""

# Step 1: Chaos planner+dispatcher tests
echo "[1/3] Running chaos planner+dispatcher tests (24 tests)..."
cargo test --package frankenterm-core \
  --test chaos_planner_dispatcher \
  --features subprocess-bridge \
  -- --nocapture 2>&1 | tee "$LOG_DIR/chaos_tests.log"
echo ""

# Step 2: Regression check against tx_e2e_scenario_matrix
echo "[2/3] Running tx E2E scenario matrix (regression check)..."
cargo test --package frankenterm-core \
  --test tx_e2e_scenario_matrix \
  -- --nocapture 2>&1 | tee "$LOG_DIR/scenario_matrix.log"
echo ""

# Step 3: Regression check against tx_correctness_suite
echo "[3/3] Running tx correctness suite (regression check)..."
cargo test --package frankenterm-core \
  --test tx_correctness_suite \
  -- --nocapture 2>&1 | tee "$LOG_DIR/correctness_suite.log"
echo ""

echo "=== All ft-1i2ge.7.2 tests passed ==="
echo "Logs: $LOG_DIR"
