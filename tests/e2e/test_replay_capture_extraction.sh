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
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/home/ubuntu}"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_capture_extraction_${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_capture_extraction_${run_id}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

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

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

resolve_timeout_bin() {
    if command -v timeout >/dev/null 2>&1; then
        TIMEOUT_BIN="timeout"
    elif command -v gtimeout >/dev/null 2>&1; then
        TIMEOUT_BIN="gtimeout"
    else
        TIMEOUT_BIN=""
    fi
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift
    set +e
    (
        cd "${ROOT_DIR}"
        env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
            rch exec -- "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e
    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${output_file}"
    fi
    return "${rc}"
}

ensure_rch_ready() {
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi
    set +e
    run_rch_cargo_logged "${RCH_SMOKE_LOG}" env CARGO_TARGET_DIR="${cargo_target_dir}" cargo check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed. See ${RCH_SMOKE_LOG}"
    fi
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"start\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

require_cmd jq
require_cmd rch
require_cmd cargo
probe_rch_workers
ensure_rch_ready

test_filter="runtime_emits_replay_capture_events_when_adapter_is_enabled"
cmd_str="env TMPDIR=$local_tmpdir ${TIMEOUT_BIN} --signal=TERM --kill-after=10 ${RCH_STEP_TIMEOUT_SECS} rch exec -- env TMPDIR=$remote_tmpdir CARGO_HOME=$cargo_home CARGO_TARGET_DIR=$cargo_target_dir cargo test -p frankenterm-core --lib $test_filter -- --nocapture"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"command\":\"$cmd_str\",\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"

set +e
(
  cd "${ROOT_DIR}"
  env TMPDIR="$local_tmpdir" "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
    rch exec -- env \
    "TMPDIR=$remote_tmpdir" \
    "CARGO_HOME=$cargo_home" \
    "CARGO_TARGET_DIR=$cargo_target_dir" \
    cargo test -p frankenterm-core --lib "$test_filter" -- --nocapture
) >"$raw_log" 2>&1
rc=$?
set -e

check_rch_fallback "$raw_log"

if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"cargo_test\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test\":\"runtime_emits_replay_capture_events_when_adapter_is_enabled\",\"error_context\":\"rch remote stall timeout\"},\"outcome\":\"failed\",\"reason_code\":\"rch_remote_stall\",\"error_code\":\"RCH-REMOTE-STALL\",\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"complete\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{\"error_context\":\"rch remote stall timeout\"},\"outcome\":\"failed\",\"reason_code\":\"rch_remote_stall\",\"error_code\":\"RCH-REMOTE-STALL\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
  fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${raw_log}"
fi

if [[ $rc -eq 0 ]]; then

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
