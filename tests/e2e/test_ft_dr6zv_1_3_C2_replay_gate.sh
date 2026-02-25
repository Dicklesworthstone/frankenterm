#!/usr/bin/env bash
# E2E test for ft-dr6zv.1.3.C2: Regression diff harness + end-to-end replay gate
#
# Verifies that:
# 1. All regression_diff unit tests pass (16 tests)
# 2. All proptest properties hold (8 property tests)
# 3. C1 facade + schema gate tests still pass (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_C2_logs"
mkdir -p "$LOG_DIR"

echo "=== ft-dr6zv.1.3.C2 E2E: RegressionDiff + ReplayGate ==="
echo "Log directory: $LOG_DIR"
echo ""

# Step 1: Unit tests for regression_diff
echo "[1/3] Running regression_diff unit tests..."
cargo test --package frankenterm-core --lib \
  -- search::regression_diff \
  --nocapture 2>&1 | tee "$LOG_DIR/unit.log"
echo ""

# Step 2: Proptest suite
echo "[2/3] Running proptest suite..."
cargo test --package frankenterm-core \
  --test proptest_regression_diff \
  -- --nocapture 2>&1 | tee "$LOG_DIR/proptest.log"
echo ""

# Step 3: C1 regression check (facade + schema gate still pass)
echo "[3/3] Running C1 regression check (facade + schema gate)..."
cargo test --package frankenterm-core --lib \
  -- search::facade search::schema_gate \
  --nocapture 2>&1 | tee "$LOG_DIR/c1_regression.log"
echo ""

echo "=== All ft-dr6zv.1.3.C2 tests passed ==="
echo "Logs: $LOG_DIR"
