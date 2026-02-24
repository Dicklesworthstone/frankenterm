#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_kernel_determinism_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_kernel_determinism"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_target_base="${CARGO_TARGET_DIR:-$ROOT_DIR/target-replay-kernel-determinism}"

with_run_id_suffix() {
  local path_base="$1"
  if [[ "$path_base" == *"${run_id}"* ]]; then
    echo "$path_base"
    return
  fi
  echo "${path_base}-${run_id}"
}

if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  cargo_target_dir="$(with_run_id_suffix "$cargo_target_base")"
else
  cargo_target_dir="$cargo_target_base"
fi
mkdir -p "$cargo_home" "$cargo_target_dir"

total_scenarios=4
pass_scenarios=0
fail_scenarios=0
suite_status=0

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_ms() {
  echo $(( $(date +%s) * 1000 ))
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
}

run_kernel_test() {
  local scenario="$1"
  local test_filter="$2"
  local decision_path="$3"
  local stdout_file="$raw_dir/scenario${scenario}.stdout.log"
  local stderr_file="$raw_dir/scenario${scenario}.stderr.log"
  local started_ms
  local ended_ms
  local duration_ms
  local rch_mode
  local reason_code

  started_ms="$(now_ms)"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"running\",\"decision_path\":\"${decision_path}\",\"inputs\":{\"test_filter\":\"${test_filter}\",\"cargo_home\":\"${cargo_home}\",\"cargo_target_dir\":\"${cargo_target_dir}\"}}"

  if rch exec -- env \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture >"$stdout_file" 2>"$stderr_file"; then
    ended_ms="$(now_ms)"
    duration_ms=$((ended_ms - started_ms))
    if grep -Fq "[RCH] local" "$stderr_file"; then
      rch_mode="local_fallback"
    else
      rch_mode="remote_offload"
    fi
    pass_scenarios=$((pass_scenarios + 1))
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"pass\",\"decision_path\":\"${decision_path}\",\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"duration_ms\":${duration_ms},\"rch_mode\":\"${rch_mode}\",\"artifacts\":{\"stdout\":\"${stdout_file#$ROOT_DIR/}\",\"stderr\":\"${stderr_file#$ROOT_DIR/}\"}}"
  else
    ended_ms="$(now_ms)"
    duration_ms=$((ended_ms - started_ms))
    if grep -Fq "No space left on device" "$stderr_file"; then
      reason_code="disk_exhausted"
    elif grep -Fq "[RCH] local" "$stderr_file"; then
      reason_code="rch_local_fallback_test_failure"
    else
      reason_code="test_failure"
    fi
    fail_scenarios=$((fail_scenarios + 1))
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"fail\",\"decision_path\":\"${decision_path}\",\"outcome\":\"fail\",\"reason_code\":\"${reason_code}\",\"error_code\":\"test_failure\",\"duration_ms\":${duration_ms},\"artifacts\":{\"stdout\":\"${stdout_file#$ROOT_DIR/}\",\"stderr\":\"${stderr_file#$ROOT_DIR/}\"}}"
    tail -n 120 "$stderr_file" >&2 || true
    return 1
  fi
}

suite_started_ms="$(now_ms)"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"start\",\"status\":\"running\",\"decision_path\":\"kernel_boot\",\"inputs\":{\"suite\":\"ft-og6q6.3.1\",\"cargo_home\":\"${cargo_home}\",\"cargo_target_dir\":\"${cargo_target_dir}\"}}"

# Scenario 1: identical trace replay should emit byte-identical decision traces
run_kernel_test "1" "recorder_replay::tests::replay_scheduler_decision_trace_is_deterministic" "scheduler.run_twice_compare" || suite_status=1

# Scenario 2: checkpoint/resume recovery path should match baseline tail
run_kernel_test "2" "recorder_replay::tests::replay_scheduler_checkpoint_resume_round_trip" "scheduler.checkpoint_resume" || suite_status=1

# Scenario 3: injected invalid checkpoint should be rejected deterministically
run_kernel_test "3" "recorder_replay::tests::replay_scheduler_rejects_invalid_checkpoint" "scheduler.failure_injection.invalid_checkpoint" || suite_status=1

# Scenario 4: virtual clock speed control invariants
run_kernel_test "4" "recorder_replay::tests::virtual_clock_speed_modes" "clock.advance" || suite_status=1

suite_ended_ms="$(now_ms)"
status="pass"
reason_code="all_scenarios_passed"
if [[ $suite_status -ne 0 ]]; then
  status="fail"
  reason_code="one_or_more_scenarios_failed"
fi

summary_json="{\"test\":\"replay_kernel\",\"scenarios\":${total_scenarios},\"pass\":${pass_scenarios},\"fail\":${fail_scenarios},\"status\":\"${status}\"}"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"complete\",\"status\":\"${status}\",\"decision_path\":\"kernel_complete\",\"outcome\":\"${status}\",\"reason_code\":\"${reason_code}\",\"duration_ms\":$((suite_ended_ms - suite_started_ms)),\"artifacts\":{\"json_log\":\"${json_log#$ROOT_DIR/}\",\"raw_dir\":\"${raw_dir#$ROOT_DIR/}\"},\"summary\":${summary_json}}"

echo "${summary_json}"

if [[ $suite_status -ne 0 ]]; then
  exit 1
fi
