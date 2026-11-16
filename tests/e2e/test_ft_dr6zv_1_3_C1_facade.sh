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

LOG_DIR="${TMPDIR:-/tmp}/ft_dr6zv_C1_logs"
mkdir -p "$LOG_DIR"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"

# ── rch infrastructure ──────────────────────────────────────────────────────
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-dr6zv-c1-${RUN_ID}"
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
echo "=== ft-dr6zv.1.3.C1 E2E: SearchFacade + SchemaGate ==="
echo "Log directory: $LOG_DIR"
echo ""

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for rch guard metadata" >&2
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "dr6zv_1_3_C1_facade" "${PROJECT_ROOT}"
ensure_rch_ready

# Step 1: Unit tests for facade + schema gate
echo "[1/3] Running facade + schema gate unit tests..."
step1_log="${LOG_DIR}/dr6zv_c1_${RUN_ID}.unit.log"
if run_rch_cargo_logged "${step1_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core --lib \
  -- search::facade search::schema_gate --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step1_log})"
  exit 1
fi
echo ""

# Step 2: Proptest suite
echo "[2/3] Running proptest suite..."
step2_log="${LOG_DIR}/dr6zv_c1_${RUN_ID}.proptest.log"
if run_rch_cargo_logged "${step2_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core \
  --test proptest_search_facade -- --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step2_log})"
  exit 1
fi
echo ""

# Step 3: Existing contract freeze (regression check)
echo "[3/3] Running search API contract freeze (regression)..."
step3_log="${LOG_DIR}/dr6zv_c1_${RUN_ID}.contract.log"
if run_rch_cargo_logged "${step3_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  test --package frankenterm-core \
  --test search_api_contract_freeze -- --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step3_log})"
  exit 1
fi
echo ""

echo "=== All ft-dr6zv.1.3.C1 tests passed ==="
echo "Logs: $LOG_DIR"
