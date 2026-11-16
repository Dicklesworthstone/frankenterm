#!/usr/bin/env bash
set -euo pipefail

# Search API Contract Freeze E2E Test (ft-dr6zv.1.3.1)
#
# Reproduction:
#   bash tests/e2e/test_search_contract_freeze.sh
# Expected:
#   - exit 0 when all scenarios pass
#   - JSON log at tests/e2e/logs/search_contract_freeze_<timestamp>.jsonl

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
GUARD_LIB="${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
mkdir -p "$LOG_DIR"

run_id="search_contract_freeze_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"
scenarios_pass=0
scenarios_fail=0
RUN_ID="${run_id}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"

now_ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
log_json() { echo "$1" >>"$json_log"; }

count_matches() {
  local pattern="$1"
  local file="$2"
  local count
  count=$(grep -c -- "${pattern}" "${file}" 2>/dev/null || true)
  if [[ -z "${count}" ]]; then
    count=0
  fi
  printf '%s\n' "${count}"
}

# ── rch infrastructure ──────────────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-search-contract-${RUN_ID}"
rch_init "${LOG_DIR}" "${RUN_ID}" "search_contract_freeze" "${ROOT_DIR}"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"step\":\"start\",\"status\":\"running\"}"

ensure_rch_ready

# ── Scenario 1: Full contract test suite ──────────────────────────────
scenario="scenario1_contract_tests"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\",\"inputs\":{\"test\":\"search_api_contract_freeze\"},\"outcome\":\"running\"}"

cargo_out="$raw_dir/${scenario}.stdout.log"
set +e
run_rch_cargo_logged "${cargo_out}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test search_api_contract_freeze -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  test_count=$(count_matches '^\s*test .* ok$' "$cargo_out")
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"inputs\":{\"test\":\"search_api_contract_freeze\"},\"reason_code\":null,\"error_code\":null,\"tests_passed\":$test_count,\"artifact_path\":\"${cargo_out#"$ROOT_DIR"/}\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"inputs\":{\"test\":\"search_api_contract_freeze\"},\"reason_code\":\"test_failure\",\"error_code\":\"exit_$rc\",\"artifact_path\":\"${cargo_out#"$ROOT_DIR"/}\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 2: Deterministic repeat-run stability ─────────────────────
scenario="scenario2_repeat_stability"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

run1="$raw_dir/${scenario}_run1.log"
run2="$raw_dir/${scenario}_run2.log"
set +e
run_rch_cargo_logged "${run1}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test search_api_contract_freeze regression_ -- --nocapture
rc1=$?
run_rch_cargo_logged "${run2}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test search_api_contract_freeze regression_ -- --nocapture
rc2=$?
set -e

if [ $rc1 -eq 0 ] && [ $rc2 -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"both_runs_pass\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"repeat_instability\",\"error_code\":\"run1=$rc1,run2=$rc2\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 3: JSON schema on disk validation ─────────────────────────
scenario="scenario3_schema_on_disk"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

schema_file="$ROOT_DIR/docs/json-schema/wa-robot-search.json"
if [ -f "$schema_file" ]; then
  if python3 -c "import json; json.load(open('$schema_file'))" 2>/dev/null || \
     jq empty "$schema_file" 2>/dev/null; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"schema_valid_json\"}"
    scenarios_pass=$((scenarios_pass + 1))
  else
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"schema_invalid_json\"}"
    scenarios_fail=$((scenarios_fail + 1))
  fi
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"schema_file_missing\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 4: Failure injection — broken schema detection ────────────
scenario="scenario4_failure_injection"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\"}"

scenario4_log="$raw_dir/${scenario}.log"
set +e
run_rch_cargo_logged "${scenario4_log}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --test search_api_contract_freeze contract_search_schema -- --nocapture
rc=$?
set -e

if [ $rc -eq 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"schema_contract_verified\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"reason_code\":\"schema_contract_broken\",\"error_code\":\"exit_$rc\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────
total=$((scenarios_pass + scenarios_fail))
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"search_contract_freeze\",\"run_id\":\"$run_id\",\"step\":\"summary\",\"status\":\"complete\",\"scenarios\":$total,\"pass\":$scenarios_pass,\"fail\":$scenarios_fail}"

echo ""
echo "=== Search Contract Freeze E2E ==="
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
