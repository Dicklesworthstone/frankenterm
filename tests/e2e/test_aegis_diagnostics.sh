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
scenario_id="aegis_diagnostics"
correlation_id="ft-3yptk-${run_id}"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
proof_ledger_file="$LOG_DIR/${run_id}.proof-ledger.jsonl"
mkdir -p "$raw_dir"
scenarios_pass=0
scenarios_fail=0

# ── rch offload variables ────────────────────────────────────────────
RCH_TARGET_DIR="target/rch-e2e-aegis-diagnostics-${run_id}"
CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"
CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
RUSTFLAGS="${RUSTFLAGS:--Cdebuginfo=0}"
export CARGO_PROFILE_DEV_DEBUG
export CARGO_PROFILE_TEST_DEBUG
export CARGO_INCREMENTAL
export RUSTFLAGS
export RCH_PROOF_LEDGER_FILE="${proof_ledger_file}"
export RCH_PROOF_LEDGER_BEAD_ID="ft-3yptk"
export RCH_PROOF_LEDGER_SCENARIO_ID="${scenario_id}"
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

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"$correlation_id\",\"step\":\"start\",\"status\":\"running\",\"proof_ledger\":\"${proof_ledger_file#"${ROOT_DIR}"/}\"}"

suite_log="$raw_dir/aegis_suite.stdout.log"
for scenario in scenario1_unit_tests scenario2_cross_module scenario3_determinism; do
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"run\",\"status\":\"running\",\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
done
set +e
run_rch_cargo_logged "$suite_log" env \
  CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG}" \
  CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG}" \
  CARGO_INCREMENTAL="${CARGO_INCREMENTAL}" \
  RUSTFLAGS="${RUSTFLAGS}" \
  CARGO_TARGET_DIR="${RCH_TARGET_DIR}" \
  cargo test -p frankenterm-core --lib aegis -- --nocapture
suite_rc=$?
set -e

# ── Scenario 1: Full unit test suite ──────────────────────────────────
scenario="scenario1_unit_tests"
test_count=$(count_matches 'test aegis_diagnostics::tests::' "$suite_log")
if [ $suite_rc -eq 0 ] && [ "$test_count" -gt 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"tests_passed\":$test_count,\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$suite_rc,tests=$test_count\",\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 2: Cross-module integration ──────────────────────────────
scenario="scenario2_cross_module"
backpressure_count=$(count_matches 'test aegis_backpressure::tests::' "$suite_log")
entropy_count=$(count_matches 'test aegis_entropy_anomaly::tests::' "$suite_log")
if [ $suite_rc -eq 0 ] && [ "$backpressure_count" -gt 0 ] && [ "$entropy_count" -gt 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"reason_code\":\"all_aegis_modules_pass\",\"backpressure_tests\":$backpressure_count,\"entropy_tests\":$entropy_count,\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$suite_rc,backpressure_tests=$backpressure_count,entropy_tests=$entropy_count\",\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Scenario 3: Determinism ──────────────────────────────────────────
scenario="scenario3_determinism"
dump_count=$(count_matches 'test aegis_diagnostics::tests::engine_dump_json ' "$suite_log")
determinism_count=$(count_matches 'test aegis_diagnostics::tests::engine_dump_json_is_deterministic ' "$suite_log")
if [ $suite_rc -eq 0 ] && [ "$dump_count" -gt 0 ] && [ "$determinism_count" -gt 0 ]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"pass\",\"outcome\":\"pass\",\"dump_tests\":$dump_count,\"determinism_tests\":$determinism_count,\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_pass=$((scenarios_pass + 1))
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario\",\"step\":\"result\",\"status\":\"fail\",\"outcome\":\"fail\",\"error_code\":\"exit_$suite_rc,dump_tests=$dump_count,determinism_tests=$determinism_count\",\"artifact_path\":\"${suite_log#"${ROOT_DIR}"/}\"}"
  scenarios_fail=$((scenarios_fail + 1))
fi

# ── Summary ────────────────────────────────────────────────────────────
total=$((scenarios_pass + scenarios_fail))
if [ "$scenarios_fail" -eq 0 ]; then
  validation_dir="$(rch_validate_proof_ledger_file "${proof_ledger_file}")"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"$correlation_id\",\"step\":\"proof_ledger_validation\",\"status\":\"pass\",\"outcome\":\"pass\",\"artifact_path\":\"${proof_ledger_file#"${ROOT_DIR}"/}\",\"validation_dir\":\"${validation_dir#"${ROOT_DIR}"/}\"}"
else
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"aegis_diagnostics\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"$correlation_id\",\"step\":\"proof_ledger_validation\",\"status\":\"skipped\",\"outcome\":\"skip\",\"artifact_path\":\"${proof_ledger_file#"${ROOT_DIR}"/}\",\"reason_code\":\"scenario_failures_present\"}"
fi
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
