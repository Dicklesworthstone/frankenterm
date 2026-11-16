#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_decision_capture_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
cargo_home="/tmp/cargo-home-replay-decision-e2e"
component="replay_decision_capture"
scenario_id="replay_decision_capture_suite"
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/tmp}"
default_cargo_target_dir="target/rch-e2e-replay-decision-capture-${run_id}"
requested_cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-}"
if [[ -n "${requested_cargo_target_dir}" && "${requested_cargo_target_dir}" != /* ]]; then
  cargo_target_dir="${requested_cargo_target_dir}"
else
  cargo_target_dir="${default_cargo_target_dir}"
fi

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
RCH_LOCAL_TMPDIR="${local_tmpdir}"
GUARD_LIB="${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${run_id}" "${component}" "${ROOT_DIR}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

assert_nonzero_tests_ran() {
    local output_file="$1"
    if grep -Eq 'running 0 tests|0 passed; 0 failed' "$output_file" 2>/dev/null; then
        echo "cargo test completed without executing any tests. See ${output_file}" >&2
        return 65
    fi
}

ensure_rch_ready

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

run_scenario() {
  local scenario_num="$1"
  local scenario_id="$2"
  local test_name="$3"

  local raw_log="$LOG_DIR/${run_id}.scenario_${scenario_num}.cargo.log"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"

  set +e
  (
    cd "${ROOT_DIR}"
    run_rch_cargo_logged "$raw_log" env \
      "TMPDIR=$remote_tmpdir" \
      "CARGO_HOME=$cargo_home" \
      "CARGO_TARGET_DIR=$cargo_target_dir" \
      cargo test -p frankenterm-core --lib "$test_name" -- --nocapture
  )
  local rc=$?
  set -e

  local meta_log="${raw_log}.rch_meta.json"
  if [[ -f "$meta_log" ]] && jq -e '.timed_out == true' "$meta_log" >/dev/null 2>&1; then
    queue_log="$(rch_timeout_queue_log "$raw_log")"
    local timeout_reason
    timeout_reason="$(jq -r '.failure_reason_code // "RCH-REMOTE-STALL"' "$meta_log")"
    if [[ -z "$timeout_reason" || "$timeout_reason" == "null" ]]; then
      timeout_reason="RCH-REMOTE-STALL"
    fi
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"rch remote stall timeout\"},\"outcome\":\"failed\",\"reason_code\":\"rch_remote_stall\",\"error_code\":\"$timeout_reason\",\"artifact_path\":\"${queue_log#"$ROOT_DIR"/}\"}"
    echo "rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s; failing closed" >&2
    return 124
  fi

  if [[ -f "$meta_log" ]] && jq -e '.fail_open_detected == true' "$meta_log" >/dev/null 2>&1; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"rch fail-open detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open\",\"error_code\":\"RCH-FAIL-OPEN\",\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
    echo "rch fail-open detected; refusing offload policy violation. See ${raw_log}" >&2
    return 1
  fi

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"cargo test failed\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return "$rc"
  fi

  if ! assert_nonzero_tests_ran "$raw_log"; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"cargo test ran zero tests\"},\"outcome\":\"failed\",\"reason_code\":\"zero_tests_executed\",\"error_code\":65,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return 65
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"decision_capture\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"error_code\":null,\"event_count\":1,\"definition_hashes\":[\"validated\"],\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
}

run_scenario 1 "ingress_tap_capture" "test_ingress_tap_impl_records_ingress_and_decision_marker"
run_scenario 2 "workflow_step_capture" "workflow_runner_emits_step_and_policy_decision_capture_events"
run_scenario 3 "policy_engine_capture" "injector_emits_policy_decision_to_replay_capture"
run_scenario 4 "capture_disabled" "test_disabled_adapter_captures_nothing"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"all_checks_passed\",\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

echo "Replay decision capture e2e passed. Logs: ${json_log#"$ROOT_DIR"/}"
