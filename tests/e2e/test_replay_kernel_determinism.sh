#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_kernel_determinism_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_kernel_determinism"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home_input="${CARGO_HOME:-.cargo-home-replay-kernel-determinism}"
if [[ "$cargo_home_input" == /* ]]; then
  cargo_home_base=".cargo-home-replay-kernel-determinism"
else
  cargo_home_base="$cargo_home_input"
fi

cargo_target_base_input="${CARGO_TARGET_DIR:-target/rch-e2e-replay-kernel-determinism}"
if [[ "$cargo_target_base_input" == /* ]]; then
  cargo_target_base="target/rch-e2e-replay-kernel-determinism"
else
  cargo_target_base="$cargo_target_base_input"
fi
cargo_git_fetch_with_cli="${CARGO_NET_GIT_FETCH_WITH_CLI:-true}"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

with_run_id_suffix() {
  local path_base="$1"
  if [[ "$path_base" == *"${run_id}"* ]]; then
    echo "$path_base"
    return
  fi
  echo "${path_base}-${run_id}"
}

cargo_home="$(with_run_id_suffix "$cargo_home_base")"
cargo_target_dir="$(with_run_id_suffix "$cargo_target_base")"
mkdir -p "$cargo_home" "$cargo_target_dir"

rch_init "$LOG_DIR" "$run_id" "replay_kernel_determinism" "$ROOT_DIR"

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
  local combined_log="$raw_dir/scenario${scenario}.combined.log"
  local started_ms
  local ended_ms
  local duration_ms
  local rch_mode
  local reason_code
  local error_code

  started_ms="$(now_ms)"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"running\",\"decision_path\":\"${decision_path}\",\"inputs\":{\"test_filter\":\"${test_filter}\",\"cargo_home\":\"${cargo_home}\",\"cargo_target_dir\":\"${cargo_target_dir}\",\"cargo_net_git_fetch_with_cli\":\"${cargo_git_fetch_with_cli}\"}}"

  set +e
  run_rch_cargo_logged "${combined_log}" \
    env CARGO_HOME="${cargo_home}" \
    CARGO_TARGET_DIR="${cargo_target_dir}" \
    CARGO_NET_GIT_FETCH_WITH_CLI="${cargo_git_fetch_with_cli}" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture
  local rc=$?
  set -e

  ended_ms="$(now_ms)"
  duration_ms=$((ended_ms - started_ms))

  if [[ $rc -eq 0 ]]; then
    rch_mode="remote_offload"
    pass_scenarios=$((pass_scenarios + 1))
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"pass\",\"decision_path\":\"${decision_path}\",\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"duration_ms\":${duration_ms},\"rch_mode\":\"${rch_mode}\",\"artifacts\":{\"combined\":\"${combined_log#"$ROOT_DIR"/}\"}}"
  else
    if grep -Fq "No space left on device" "$combined_log"; then
      reason_code="disk_exhausted"
      error_code="disk_no_space_left"
    elif grep -Fq "failed to load source for dependency" "$combined_log" || grep -Eq "revision [[:alnum:]]+ not found" "$combined_log"; then
      reason_code="dependency_fetch_failed"
      error_code="cargo_git_dependency_revision_not_found"
    elif grep -Fq "[RCH] remote" "$combined_log"; then
      reason_code="remote_execution_failure"
      error_code="rch_remote_command_failed"
    else
      reason_code="test_failure"
      error_code="test_failure"
    fi

    rch_mode="remote_offload"
    fail_scenarios=$((fail_scenarios + 1))
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"${scenario}\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"run_test\",\"status\":\"fail\",\"decision_path\":\"${decision_path}\",\"outcome\":\"fail\",\"reason_code\":\"${reason_code}\",\"error_code\":\"${error_code}\",\"duration_ms\":${duration_ms},\"rch_mode\":\"${rch_mode}\",\"artifacts\":{\"combined\":\"${combined_log#"$ROOT_DIR"/}\"}}"
    tail -n 120 "$combined_log" >&2 || true
    return 1
  fi
}

ensure_rch_ready

suite_started_ms="$(now_ms)"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"start\",\"status\":\"running\",\"decision_path\":\"kernel_boot\",\"inputs\":{\"suite\":\"ft-og6q6.3.1\",\"cargo_home\":\"${cargo_home}\",\"cargo_target_dir\":\"${cargo_target_dir}\",\"cargo_net_git_fetch_with_cli\":\"${cargo_git_fetch_with_cli}\"}}"

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
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_kernel\",\"scenario_id\":\"$scenario_id\",\"correlation_id\":\"${run_id}\",\"run_id\":\"${run_id}\",\"step\":\"complete\",\"status\":\"${status}\",\"decision_path\":\"kernel_complete\",\"outcome\":\"${status}\",\"reason_code\":\"${reason_code}\",\"duration_ms\":$((suite_ended_ms - suite_started_ms)),\"artifacts\":{\"json_log\":\"${json_log#"$ROOT_DIR"/}\",\"raw_dir\":\"${raw_dir#"$ROOT_DIR"/}\"},\"summary\":${summary_json}}"

echo "${summary_json}"

if [[ $suite_status -ne 0 ]]; then
  exit 1
fi
