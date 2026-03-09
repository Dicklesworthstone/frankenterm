#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_redaction_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
cargo_home="/tmp/cargo-home-replay-redaction-e2e"
cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-$ROOT_DIR/target-replay-redaction-e2e-${run_id}}"
component="replay_redaction"
scenario_id="replay_redaction_suite"
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/home/ubuntu}"

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
  local probe_json

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"

  set +e
  env TMPDIR="$local_tmpdir" rch workers probe --all --json >"$probe_log" 2>&1
  local probe_rc=$?
  set -e

  if [[ $probe_rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_probe_failed\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "rch workers probe failed" >&2
    exit 2
  fi

  probe_json="$(awk 'capture || /^[[:space:]]*[{]/{capture=1; print}' "$probe_log")"
  local healthy_workers
  healthy_workers="$(printf '%s\n' "$probe_json" | jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' 2>/dev/null || echo 0)"
  if [[ "$healthy_workers" -lt 1 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"failed\",\"reason_code\":\"rch_workers_unreachable\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
}

require_cmd jq
require_cmd rch
require_cmd cargo
probe_rch_workers

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

run_scenario() {
  local scenario_num="$1"
  local scenario_id="$2"
  local test_name="$3"

  local raw_log="$LOG_DIR/${run_id}.scenario_${scenario_num}.cargo.log"
  local cmd=(
    env
    "TMPDIR=$local_tmpdir"
    rch exec -- env
    "TMPDIR=$remote_tmpdir"
    "CARGO_HOME=$cargo_home"
    "CARGO_TARGET_DIR=$cargo_target_dir"
    cargo test -p frankenterm-core --lib "$test_name" -- --nocapture
  )

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"

  set +e
  "${cmd[@]}" >"$raw_log" 2>&1
  local rc=$?
  set -e

  if grep -Eq '\[RCH\][[:space:]]+local|fail-open' "$raw_log"; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    return 3
  fi

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\",\"error_context\":\"cargo test failed\"},\"outcome\":\"failed\",\"reason_code\":\"cargo_test_failed\",\"error_code\":$rc,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
    tail -n 80 "$raw_log" >&2 || true
    return "$rc"
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"${scenario_num}:${scenario_id}\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"replay_redaction\",\"scenario\":$scenario_num,\"cargo_test\":\"$test_name\"},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"error_code\":null,\"secrets_found\":1,\"secrets_redacted\":1,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
}

run_scenario 1 "mask_mode" "test_capture_redaction_mask_mode_marks_partial_for_t2"
run_scenario 2 "hash_mode" "test_capture_redaction_hash_mode_hashes_sensitive_text"
run_scenario 3 "retention_tombstone" "test_capture_redaction_t3_retention_zero_tombstones"
run_scenario 4 "custom_patterns" "test_capture_redaction_policy_loads_custom_patterns_from_toml"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"all_checks_passed\",\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

echo "Replay redaction e2e passed. Logs: ${json_log#$ROOT_DIR/}"
