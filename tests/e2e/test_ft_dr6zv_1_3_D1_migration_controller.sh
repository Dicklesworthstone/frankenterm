#!/usr/bin/env bash
# E2E test for ft-dr6zv.1.3.D1: Legacy path retirement + migration controller
#
# Verifies that:
# 1. All migration_controller unit tests pass (22 tests)
# 2. All proptest properties hold (8 property tests)
# 3. C1 + C2 tests still pass (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_D1_logs"
mkdir -p "$LOG_DIR"

echo "=== ft-dr6zv.1.3.D1 E2E: MigrationController + RetirementGate ==="
echo "Log directory: $LOG_DIR"
echo ""

# Step 1: Unit tests for migration_controller
echo "[1/4] Running migration_controller unit tests..."
cargo test --package frankenterm-core --lib \
  -- search::migration_controller \
  --nocapture 2>&1 | tee "$LOG_DIR/unit.log"
echo ""

# Step 2: Proptest suite
echo "[2/4] Running proptest suite..."
cargo test --package frankenterm-core \
  --test proptest_migration_controller \
  -- --nocapture 2>&1 | tee "$LOG_DIR/proptest.log"
echo ""

# Step 3: C1 + C2 regression check
echo "[3/4] Running C1 regression check (facade + schema gate)..."
cargo test --package frankenterm-core --lib \
  -- search::facade search::schema_gate \
  --nocapture 2>&1 | tee "$LOG_DIR/c1_regression.log"
echo ""

echo "[4/4] Running C2 regression check (regression_diff)..."
cargo test --package frankenterm-core --lib \
  -- search::regression_diff \
  --nocapture 2>&1 | tee "$LOG_DIR/c2_regression.log"
echo ""

echo "=== All ft-dr6zv.1.3.D1 tests passed ==="
echo "Logs: $LOG_DIR"
