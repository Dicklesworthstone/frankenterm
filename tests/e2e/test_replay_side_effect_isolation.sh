#!/usr/bin/env bash
set -euo pipefail

# Replay Side-Effect Isolation E2E Test (ft-og6q6.3.3)
#
# Reproduction:
#   bash tests/e2e/test_replay_side_effect_isolation.sh
# Expected:
#   - exit 0 when all scenarios pass
#   - JSON log at tests/e2e/logs/replay_side_effect_isolation_<timestamp>.jsonl

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_side_effect_isolation_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"
scenarios_pass=0
scenarios_fail=0

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
RCH_TARGET_DIR="target/rch-e2e-replay-side-effect-isolation-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_side_effect_isolation_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_side_effect_isolation_${RUN_ID}.smoke.log"

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

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"step\":\"start\",\"status\":\"running\"}"

ensure_rch_ready

# ── Scenario 1: Full unit test suite ──────────────────────────────────
scenario="scenario1_unit_tests"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.stdout.log" test -p frankenterm-core replay_side_effect_barrier -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  test_count=$(grep -c 'test replay_side_effect_barrier::tests::' "$raw_dir/${scenario}.stdout.log" || echo "0")
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"tests_passed\":$test_count}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 2: Property-based tests ─────────────────────────────────
scenario="scenario2_proptest"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.stdout.log" test -p frankenterm-core --test proptest_replay_side_effect_barrier -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"all_property_tests_pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 3: Determinism (repeat run) ─────────────────────────────
scenario="scenario3_determinism"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}_run1.log" test -p frankenterm-core replay_side_effect_barrier::tests::replay_barrier_blocks_all_effect_types -- --nocapture
rc1=$?
run_rch_cargo_logged "$raw_dir/${scenario}_run2.log" test -p frankenterm-core replay_side_effect_barrier::tests::replay_barrier_blocks_all_effect_types -- --nocapture
rc2=$?
set -e

if [ $rc1 -eq 0 ] && [ $rc2 -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"run1=$rc1,run2=$rc2\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 4: Isolation completeness (P-09 property) ──────────────
scenario="scenario4_isolation_completeness"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

set +e
run_rch_cargo_logged "$raw_dir/${scenario}.stdout.log" test -p frankenterm-core --test proptest_replay_side_effect_barrier no_effects_escape_replay -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"effects_leaked\":0,\"reason_code\":\"p09_isolation_verified\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────
total=$((scenarios_pass + scenarios_fail))
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_side_effect_isolation\",\"run_id\":\"$run_id\",\"step\":\"summary\",\"status\":\"complete\",\"scenarios\":$total,\"pass\":$scenarios_pass,\"fail\":$scenarios_fail}"

echo ""
echo "=== Replay Side-Effect Isolation E2E ==="
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
