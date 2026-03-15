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
RCH_TARGET_DIR="target/rch-e2e-dr6zv-c2-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/dr6zv_c2_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/dr6zv_c2_${RUN_ID}.smoke.log"

fatal() { echo "FATAL: $1" >&2; exit 1; }
run_rch() { TMPDIR=/tmp rch "$@"; }
run_rch_cargo() { run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"; }
probe_has_reachable_workers() { grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"; }

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"; shift
    set +e; ( cd "${PROJECT_ROOT}"; run_rch_cargo "$@" ) >"${output_file}" 2>&1; local rc=$?; set -e
    check_rch_fallback "${output_file}"; return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e; run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1; local probe_rc=$?; set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e; run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1; local smoke_rc=$?; set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed. See ${RCH_SMOKE_LOG}"
    fi
}

# ── preflight ───────────────────────────────────────────────────────────────
echo "=== ft-dr6zv.1.3.C2 E2E: RegressionDiff + ReplayGate ==="
echo "Log directory: $LOG_DIR"
echo ""

ensure_rch_ready

# Step 1: Unit tests for regression_diff
echo "[1/3] Running regression_diff unit tests..."
step1_log="${LOG_DIR}/dr6zv_c2_${RUN_ID}.unit.log"
if run_rch_cargo_logged "${step1_log}" test --package frankenterm-core --lib \
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
if run_rch_cargo_logged "${step2_log}" test --package frankenterm-core \
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
if run_rch_cargo_logged "${step3_log}" test --package frankenterm-core --lib \
  -- search::facade search::schema_gate --nocapture; then
  echo "  PASS"
else
  echo "  FAIL (see ${step3_log})"
  exit 1
fi
echo ""

echo "=== All ft-dr6zv.1.3.C2 tests passed ==="
echo "Logs: $LOG_DIR"
