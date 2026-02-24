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
cargo_target_dir="$ROOT_DIR/target-replay-capture-e2e-${run_id}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\"}" >>"$json_log"

cmd=(
  rch exec -- env
  CARGO_HOME="$cargo_home"
  CARGO_TARGET_DIR="$cargo_target_dir"
  cargo test -p frankenterm-core --lib runtime_emits_replay_capture_events_when_adapter_is_enabled -- --nocapture
)
cmd_str="${cmd[*]}"

echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"cargo_test\",\"status\":\"running\",\"command\":\"$cmd_str\",\"raw_log\":\"${raw_log#$ROOT_DIR/}\"}" >>"$json_log"

set +e
"${cmd[@]}" >"$raw_log" 2>&1
rc=$?
set -e

if [[ $rc -eq 0 ]]; then
  echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"cargo_test\",\"status\":\"passed\",\"assertions\":[\"runtime emits egress replay capture events\",\"runtime emits lifecycle replay capture events\",\"captured events include deterministic event_id values\"],\"raw_log\":\"${raw_log#$ROOT_DIR/}\"}" >>"$json_log"
  echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"passed\"}" >>"$json_log"
  echo "Replay capture extraction e2e passed. Logs: ${json_log#$ROOT_DIR/}"
  exit 0
fi

echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"cargo_test\",\"status\":\"failed\",\"error_code\":\"cargo_test_failed\",\"exit_code\":$rc,\"raw_log\":\"${raw_log#$ROOT_DIR/}\"}" >>"$json_log"
echo "{\"ts\":\"$(now_ts)\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"failed\"}" >>"$json_log"

echo "Replay capture extraction e2e failed. Logs: ${json_log#$ROOT_DIR/}" >&2
tail -n 80 "$raw_log" >&2 || true
exit "$rc"
