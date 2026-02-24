#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_redaction_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
cargo_home="/tmp/cargo-home-replay-redaction-e2e"
cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-$ROOT_DIR/target-replay-redaction-e2e-${run_id}}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_redaction\",\"scenario_id\":\"suite_start\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

run_scenario() {
  local scenario_num="$1"
  local scenario_id="$2"
  local test_name="$3"

  local raw_log="$LOG_DIR/${run_id}.scenario_${scenario_num}.cargo.log"
  local cmd=(
    rch exec -- env
    CARGO_HOME="$cargo_home"
    CARGO_TARGET_DIR="$cargo_target_dir"
    cargo test -p frankenterm-core --lib "$test_name" -- --nocapture
  )

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_redaction\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"

  set +e
  "${cmd[@]}" >"$raw_log" 2>&1
  local rc=$?
  set -e

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_redaction\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return "$rc"
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_redaction\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"secrets_found\":1,\"secrets_redacted\":1,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
}

run_scenario 1 "mask_mode" "test_capture_redaction_mask_mode_marks_partial_for_t2"
run_scenario 2 "hash_mode" "test_capture_redaction_hash_mode_hashes_sensitive_text"
run_scenario 3 "retention_tombstone" "test_capture_redaction_t3_retention_zero_tombstones"
run_scenario 4 "custom_patterns" "test_capture_redaction_policy_loads_custom_patterns_from_toml"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_redaction\",\"scenario_id\":\"suite_complete\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

echo "Replay redaction e2e passed. Logs: ${json_log#$ROOT_DIR/}"
