#!/usr/bin/env bash
set -euo pipefail

# Aegis PAC-Bayesian Backpressure E2E Test (ft-l5em3.3)
#
# Reproduction:
#   bash tests/e2e/test_aegis_backpressure.sh
# Expected:
#   - exit 0 when all scenarios pass
#   - JSON log at tests/e2e/logs/aegis_backpressure_<timestamp>.jsonl

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="aegis_backpressure_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"
scenarios_pass=0
scenarios_fail=0

# ── rch offload variables ────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-aegis-backpressure-${run_id}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/${run_id}.smoke.log"

# ── helpers ───────────────────────────────────────────────────────────
now_ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

log_json() { echo "$1" >>"$json_log"; }

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift
    set +e
    (
        cd "${ROOT_DIR}"
        run_rch_cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e
    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e
    run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
    local smoke_rc=$?
    set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

# ── preflight ─────────────────────────────────────────────────────────
ensure_rch_ready

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"step\":\"start\",\"status\":\"running\"}"

# ── Scenario 1: Full unit test suite ──────────────────────────────────
scenario="scenario1_unit_tests"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

cargo_out="$raw_dir/${scenario}.stdout.log"

set +e
run_rch_cargo_logged "$cargo_out" test -p frankenterm-core aegis_backpressure -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  test_count=$(grep -c 'test aegis_backpressure::tests::' "$cargo_out" || echo "0")
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"tests_passed\":$test_count}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"test_failure\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 2: Repeat-run determinism ────────────────────────────────
scenario="scenario2_determinism"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

run1="$raw_dir/${scenario}_run1.log"
run2="$raw_dir/${scenario}_run2.log"

set +e
run_rch_cargo_logged "$run1" test -p frankenterm-core aegis_backpressure::tests::deterministic -- --nocapture
rc1=$?
run_rch_cargo_logged "$run2" test -p frankenterm-core aegis_backpressure::tests::deterministic -- --nocapture
rc2=$?
set -e

if [ $rc1 -eq 0 ] && [ $rc2 -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"both_runs_pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"repeat_instability\",\"error_code\":\"run1=$rc1,run2=$rc2\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 3: Starvation guard correctness ──────────────────────────
scenario="scenario3_starvation_guard"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.log" test -p frankenterm-core aegis_backpressure::tests::starvation_guard -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 4: 100-pane stress test ──────────────────────────────────
scenario="scenario4_100_pane_stress"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.log" test -p frankenterm-core aegis_backpressure::tests::many_panes_stress -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────
total=$((scenarios_pass + scenarios_fail))
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_backpressure\",\"run_id\":\"$run_id\",\"step\":\"summary\",\"status\":\"complete\",\"scenarios\":$total,\"pass\":$scenarios_pass,\"fail\":$scenarios_fail}"

echo ""
echo "=== Aegis PAC-Bayesian Backpressure E2E ==="
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
