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
cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-$ROOT_DIR/target-replay-capture-e2e-${run_id}}"
component="replay_capture_extraction"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"prereq_check\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"command\":\"$cmd\"},\"outcome\":\"failed\",\"reason_code\":\"missing_prerequisite\",\"error_code\":\"E2E-PREREQ\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
    echo "missing required command: $cmd" >&2
    exit 1
  fi
}

probe_rch_workers() {
  local probe_log="$LOG_DIR/${run_id}.rch_probe.json"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"

  set +e
  rch workers probe --all --json >"$probe_log" 2>&1
  local probe_rc=$?
  set -e

  if [[ $probe_rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_probe_failed\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "rch workers probe failed" >&2
    exit 2
  fi

  local healthy_workers
  healthy_workers="$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "$probe_log" 2>/dev/null || echo 0)"
  if [[ "$healthy_workers" -lt 1 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"failed\",\"reason_code\":\"rch_workers_unreachable\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"start\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

require_cmd jq
require_cmd rch
require_cmd cargo
probe_rch_workers

cmd=(
  rch exec -- env
  CARGO_HOME="$cargo_home"
  CARGO_TARGET_DIR="$cargo_target_dir"
  cargo test -p frankenterm-core --lib runtime_emits_replay_capture_events_when_adapter_is_enabled -- --nocapture
)
cmd_str="${cmd[*]}"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"command\":\"$cmd_str\",\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"

set +e
"${cmd[@]}" >"$raw_log" 2>&1
rc=$?
set -e

if [[ $rc -eq 0 ]]; then
  if grep -Eq '\[RCH\][[:space:]]+local|fail-open' "$raw_log"; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\",\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\"},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"assertions\":[\"runtime emits egress replay capture events\",\"runtime emits lifecycle replay capture events\",\"captured events include deterministic event_id values\"],\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
  echo "Replay capture extraction e2e passed. Logs: ${json_log#$ROOT_DIR/}"
  exit 0
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\",\"error_context\":\"see cargo raw log\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"cargo test command failed\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

echo "Replay capture extraction e2e failed. Logs: ${json_log#$ROOT_DIR/}" >&2
tail -n 80 "$raw_log" >&2 || true
exit "$rc"
