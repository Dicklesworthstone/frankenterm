#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_artifact_reader_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_artifact_reader"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="/tmp/cargo-home-replay-artifact-reader"
cargo_target_dir="$ROOT_DIR/target-replay-artifact-reader"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
}

run_reader_test() {
  local scenario="$1"
  local test_filter="$2"
  local stdout_file="$raw_dir/scenario${scenario}.stdout.log"
  local stderr_file="$raw_dir/scenario${scenario}.stderr.log"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"test_filter\":\"$test_filter\"}}"

  if rch exec -- env \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture >"$stdout_file" 2>"$stderr_file"; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"test\":\"artifact_reader\",\"version\":\"ftreplay.v1\",\"integrity\":\"pass\",\"compression\":\"none|gzip|zstd\",\"artifact_path\":\"${stdout_file#$ROOT_DIR/}\"}"
  else
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"${scenario}\",\"step\":\"run_test\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"test\":\"artifact_reader\",\"version\":\"ftreplay.v1\",\"integrity\":\"fail\",\"reason_code\":\"test_failure\",\"artifact_path\":\"${stderr_file#$ROOT_DIR/}\"}"
    tail -n 120 "$stderr_file" >&2 || true
    exit 1
  fi
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"run_id\":\"$run_id\"}"

# Scenario 1: write/read and parseable baseline
run_reader_test "1" "replay_fixture_harvest::tests::artifact_reader_open_reads_v1_artifact"

# Scenario 2: corruption/integrity mismatch
run_reader_test "2" "replay_fixture_harvest::tests::artifact_reader_reports_integrity_mismatch"

# Scenario 3: migration path (v0 -> v1)
run_reader_test "3" "replay_fixture_harvest::tests::artifact_reader_open_applies_v0_to_v1_migration"

# Scenario 4: future version incompatible path
run_reader_test "4" "replay_fixture_harvest::tests::artifact_reader_open_rejects_unknown_future_schema"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_reader\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"pass\",\"run_id\":\"$run_id\"}"

echo "Replay artifact reader e2e passed. Logs: ${json_log#$ROOT_DIR/}"
