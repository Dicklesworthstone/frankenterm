#!/usr/bin/env bash
# E2E test for ft-dr6zv.1.3.C1: Compatibility facade + schema preservation gate
#
# Verifies that:
# 1. All facade unit tests pass (28 tests)
# 2. All schema gate unit tests pass (26 tests)
# 3. All proptest properties hold (12 property tests)
# 4. No regressions in existing search API contract freeze
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_C1_logs"
mkdir -p "$LOG_DIR"

echo "=== ft-dr6zv.1.3.C1 E2E: SearchFacade + SchemaGate ==="
echo "Log directory: $LOG_DIR"
echo ""

# Step 1: Unit tests for facade + schema gate
echo "[1/3] Running facade + schema gate unit tests..."
cargo test --package frankenterm-core --lib \
  -- search::facade search::schema_gate \
  --nocapture 2>&1 | tee "$LOG_DIR/unit.log"
echo ""

# Step 2: Proptest suite
echo "[2/3] Running proptest suite..."
cargo test --package frankenterm-core \
  --test proptest_search_facade \
  -- --nocapture 2>&1 | tee "$LOG_DIR/proptest.log"
echo ""

# Step 3: Existing contract freeze (regression check)
echo "[3/3] Running search API contract freeze (regression)..."
cargo test --package frankenterm-core \
  --test search_api_contract_freeze \
  -- --nocapture 2>&1 | tee "$LOG_DIR/contract.log"
echo ""

echo "=== All ft-dr6zv.1.3.C1 tests passed ==="
echo "Logs: $LOG_DIR"
