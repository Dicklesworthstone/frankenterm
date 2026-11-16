#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_capture_extraction_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="runtime_replay_capture_adapter"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_log="$LOG_DIR/${run_id}.cargo.log"
cargo_home="/tmp/cargo-home-replay-capture-e2e"
component="replay_capture_extraction"
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/tmp}"
default_cargo_target_dir="target/rch-e2e-replay-capture-extraction-${run_id}"
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

fatal() { rch_fatal "$1"; }

assert_nonzero_tests_ran() {
    local output_file="$1"
    if grep -Eq 'running 0 tests|0 passed; 0 failed' "$output_file" 2>/dev/null; then
        fatal "cargo test completed without executing any tests. See ${output_file}"
    fi
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"start\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

ensure_rch_ready

test_filter="runtime_with_replay_capture_adapter_shuts_down_cleanly"
cmd_str="run_rch_cargo_logged env TMPDIR=$remote_tmpdir CARGO_HOME=$cargo_home CARGO_TARGET_DIR=$cargo_target_dir cargo test -p frankenterm-core --lib $test_filter -- --nocapture"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"command\":\"$cmd_str\",\"test\":\"$test_filter\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"

set +e
(
  cd "${ROOT_DIR}"
  run_rch_cargo_logged "$raw_log" env \
    "TMPDIR=$remote_tmpdir" \
    "CARGO_HOME=$cargo_home" \
    "CARGO_TARGET_DIR=$cargo_target_dir" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture
)
rc=$?
set -e

meta_log="${raw_log}.rch_meta.json"
if [[ -f "$meta_log" ]] && jq -e '.timed_out == true' "$meta_log" >/dev/null 2>&1; then
  queue_log="$(rch_timeout_queue_log "$raw_log")"
  timeout_reason="$(jq -r '.failure_reason_code // "RCH-REMOTE-STALL"' "$meta_log")"
  if [[ -z "$timeout_reason" || "$timeout_reason" == "null" ]]; then
    timeout_reason="RCH-REMOTE-STALL"
  fi
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"$test_filter\",\"error_context\":\"rch remote stall timeout\"},\"outcome\":\"failed\",\"reason_code\":\"rch_remote_stall\",\"error_code\":\"$timeout_reason\",\"artifact_path\":\"${queue_log#"$ROOT_DIR"/}\"}"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"rch remote stall timeout\"},\"outcome\":\"failed\",\"reason_code\":\"rch_remote_stall\",\"error_code\":\"$timeout_reason\",\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"
  fatal "${timeout_reason}: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${queue_log}"
fi

if [[ -f "$meta_log" ]] && jq -e '.fail_open_detected == true' "$meta_log" >/dev/null 2>&1; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"$test_filter\",\"error_context\":\"rch fail-open detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open\",\"error_code\":\"RCH-FAIL-OPEN\",\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"rch fail-open detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open\",\"error_code\":\"RCH-FAIL-OPEN\",\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"
  fatal "RCH-FAIL-OPEN: refusing offload policy violation. See ${raw_log}"
fi

assert_nonzero_tests_ran "$raw_log"

if [[ $rc -eq 0 ]]; then

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"$test_filter\"},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"assertions\":[\"runtime emits egress replay capture events\",\"runtime emits lifecycle replay capture events\",\"captured events include deterministic event_id values\"],\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"
  echo "Replay capture extraction e2e passed. Logs: ${json_log#"$ROOT_DIR"/}"
  exit 0
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"$test_filter\",\"error_context\":\"see cargo raw log\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#"$ROOT_DIR"/}\"}"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"cargo test command failed\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

echo "Replay capture extraction e2e failed. Logs: ${json_log#"$ROOT_DIR"/}" >&2
tail -n 80 "$raw_log" >&2 || true
exit "$rc"
