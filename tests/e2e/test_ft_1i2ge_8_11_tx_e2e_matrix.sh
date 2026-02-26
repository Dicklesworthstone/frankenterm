#!/usr/bin/env bash
# E2E test for ft-1i2ge.8.11: Deterministic E2E scenario matrix for tx run/rollback flows
#
# Verifies that:
# 1. All 19 scenario matrix tests pass (9 core scenarios + 10 cross-scenario checks)
# 2. Existing tx_correctness_suite still passes (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

LOG_DIR="${TMPDIR:-/tmp}/ft_1i2ge_H11_logs"
mkdir -p "$LOG_DIR"

echo "=== ft-1i2ge.8.11 E2E: Tx Scenario Matrix ==="
echo "Log directory: $LOG_DIR"
echo ""

# Step 1: E2E scenario matrix
echo "[1/2] Running tx E2E scenario matrix (19 tests)..."
cargo test --package frankenterm-core \
  --test tx_e2e_scenario_matrix \
  -- --nocapture 2>&1 | tee "$LOG_DIR/scenario_matrix.log"
echo ""

# Step 2: Regression check against existing tx correctness suite
echo "[2/2] Running tx correctness suite (regression check)..."
cargo test --package frankenterm-core \
  --test tx_correctness_suite \
  -- --nocapture 2>&1 | tee "$LOG_DIR/correctness_suite.log"
echo ""

echo "=== All ft-1i2ge.8.11 tests passed ==="
echo "Logs: $LOG_DIR"
