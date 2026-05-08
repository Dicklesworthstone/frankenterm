#!/usr/bin/env bash
set -euo pipefail

# Aegis Diagnostics Integration E2E Test (ft-l5em3.5)
#
# Reproduction:
#   bash tests/e2e/test_aegis_diagnostics.sh
# Expected:
#   - exit 0 when all scenarios pass
#   - JSON log at tests/e2e/logs/aegis_diagnostics_<timestamp>.jsonl

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="aegis_diagnostics_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"
scenarios_pass=0
scenarios_fail=0

# ── rch offload variables ────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-aegis-diagnostics-${run_id}"
GUARD_LIB="${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${run_id}" "aegis_diagnostics" "${ROOT_DIR}"

# ── helpers ───────────────────────────────────────────────────────────
now_ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
log_json() { echo "$1" >>"$json_log"; }

count_matches() {
    local pattern="$1"
    local file="$2"
    local count
    count=$(grep -c -- "$pattern" "$file") || {
        local rc=$?
        if [[ ${rc} -eq 1 ]]; then
            count=0
        else
            return "${rc}"
        fi
    }
    printf '%s\n' "$count"
}

# ── preflight ─────────────────────────────────────────────────────────
ensure_rch_ready

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"step\":\"start\",\"status\":\"running\"}"

# ── Scenario 1: Full unit test suite ──────────────────────────────────
scenario="scenario1_unit_tests"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.stdout.log" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core aegis_diagnostics -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  test_count=$(count_matches 'test aegis_diagnostics::tests::' "$raw_dir/${scenario}.stdout.log")
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"tests_passed\":$test_count}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 2: Cross-module integration ──────────────────────────────
scenario="scenario2_cross_module"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}_backpressure.log" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core aegis_backpressure -- --nocapture
rc1=$?
run_rch_cargo_logged "$raw_dir/${scenario}_entropy.log" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core aegis_entropy_anomaly -- --nocapture
rc2=$?
set -e

if [ $rc1 -eq 0 ] && [ $rc2 -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"all_aegis_modules_pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"backpressure=$rc1,entropy=$rc2\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 3: Determinism ──────────────────────────────────────────
scenario="scenario3_determinism"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}_run1.log" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core aegis_diagnostics::tests::engine_dump_json -- --nocapture
rc1=$?
run_rch_cargo_logged "$raw_dir/${scenario}_run2.log" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core aegis_diagnostics::tests::engine_dump_json -- --nocapture
rc2=$?
set -e

if [ $rc1 -eq 0 ] && [ $rc2 -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"run1=$rc1,run2=$rc2\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────
total=$((scenarios_pass + scenarios_fail))
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"step\":\"summary\",\"status\":\"complete\",\"scenarios\":$total,\"pass\":$scenarios_pass,\"fail\":$scenarios_fail}"

echo ""
echo "=== Aegis Diagnostics Integration E2E ==="
echo "Run:       $run_id"
echo "Scenarios: $total  pass=$scenarios_pass  fail=$scenarios_fail"
echo "Log:       $json_log"
echo ""

if [ "$scenarios_fail" -gt 0 ]; then
  echo "FAILED: $scenarios_fail scenario(s) failed"
  exit 1
fi

echo "ALL SCENARIOS PASSED"
exit 0
