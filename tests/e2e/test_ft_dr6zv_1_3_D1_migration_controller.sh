#!/usr/bin/env bash
# E2E test for ft-dr6zv.1.3.D1: Legacy path retirement + migration controller
#
# Verifies that:
# 1. All migration_controller unit tests pass (22 tests)
# 2. All proptest properties hold (8 property tests)
# 3. C1 + C2 tests still pass (regression check)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_D1_logs"
mkdir -p "$LOG_DIR"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"

# ── rch infrastructure ──────────────────────────────────────────────────────
CARGO_TARGET_DIR="target/rch-e2e-dr6zv-d1-${RUN_ID}"
GUARD_LIB="${SCRIPT_DIR}/lib_rch_guards.sh"

run_cargo_step() {
    local output_file="$1"
    shift
    run_rch_cargo_logged "${output_file}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
}

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"

# ── preflight ───────────────────────────────────────────────────────────────
echo "=== ft-dr6zv.1.3.D1 E2E: MigrationController + RetirementGate ==="
echo "Log directory: $LOG_DIR"
echo ""

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for rch metadata artifacts" >&2
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "dr6zv_d1"
ensure_rch_ready

# Step 1: Unit tests for migration_controller
echo "[1/4] Running migration_controller unit tests..."
step1_log="${LOG_DIR}/dr6zv_d1_${RUN_ID}.unit.log"
if run_cargo_step "${step1_log}" test --package frankenterm-core --lib \
  -- search::migration_controller --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step1_log})"
  exit 1
fi
echo ""

# Step 2: Proptest suite
echo "[2/4] Running proptest suite..."
step2_log="${LOG_DIR}/dr6zv_d1_${RUN_ID}.proptest.log"
if run_cargo_step "${step2_log}" test --package frankenterm-core \
  --test proptest_migration_controller -- --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step2_log})"
  exit 1
fi
echo ""

# Step 3: C1 + C2 regression check
echo "[3/4] Running C1 regression check (facade + schema gate)..."
step3_log="${LOG_DIR}/dr6zv_d1_${RUN_ID}.c1_regression.log"
if run_cargo_step "${step3_log}" test --package frankenterm-core --lib \
  -- search::facade search::schema_gate --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step3_log})"
  exit 1
fi
echo ""

echo "[4/4] Running C2 regression check (regression_diff)..."
step4_log="${LOG_DIR}/dr6zv_d1_${RUN_ID}.c2_regression.log"
if run_cargo_step "${step4_log}" test --package frankenterm-core --lib \
  -- search::regression_diff --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step4_log})"
  exit 1
fi
echo ""

echo "=== All ft-dr6zv.1.3.D1 tests passed ==="
echo "Logs: $LOG_DIR"
