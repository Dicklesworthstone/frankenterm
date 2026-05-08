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

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_C2_logs"
mkdir -p "$LOG_DIR"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"

# ── rch infrastructure ──────────────────────────────────────────────────────
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-dr6zv-c2-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${SCRIPT_DIR}/lib_rch_guards.sh"

# ── preflight ───────────────────────────────────────────────────────────────
echo "=== ft-dr6zv.1.3.C2 E2E: RegressionDiff + ReplayGate ==="
echo "Log directory: $LOG_DIR"
echo ""

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for rch guard metadata" >&2
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "dr6zv_1_3_C2_replay_gate" "${PROJECT_ROOT}"
ensure_rch_ready

# Step 1: Unit tests for regression_diff
echo "[1/3] Running regression_diff unit tests..."
step1_log="${LOG_DIR}/dr6zv_c2_${RUN_ID}.unit.log"
if run_rch_cargo_logged "${step1_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core --lib \
  -- search::regression_diff --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step1_log})"
  exit 1
fi
echo ""

# Step 2: Proptest suite
echo "[2/3] Running proptest suite..."
step2_log="${LOG_DIR}/dr6zv_c2_${RUN_ID}.proptest.log"
if run_rch_cargo_logged "${step2_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core \
  --test proptest_regression_diff -- --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step2_log})"
  exit 1
fi
echo ""

# Step 3: C1 regression check (facade + schema gate still pass)
echo "[3/3] Running C1 regression check (facade + schema gate)..."
step3_log="${LOG_DIR}/dr6zv_c2_${RUN_ID}.c1_regression.log"
if run_rch_cargo_logged "${step3_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core --lib \
  -- search::facade search::schema_gate --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step3_log})"
  exit 1
fi
echo ""

echo "=== All ft-dr6zv.1.3.C2 tests passed ==="
echo "Logs: $LOG_DIR"
