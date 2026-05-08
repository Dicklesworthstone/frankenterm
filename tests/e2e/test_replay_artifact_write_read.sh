#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_artifact_write_read_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_artifact_write_read"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"
component="replay_artifact_write_read"

cargo_home="/tmp/cargo-home-replay-artifact-write-read"
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/home/ubuntu}"
cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-$remote_tmpdir/target-replay-artifact-write-read-${run_id}}"
work_dir="$ROOT_DIR/tests/e2e/tmp/${run_id}"
mkdir -p "$work_dir"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_DAEMON_STATUS_LOG="${LOG_DIR}/${run_id}.rch_daemon_status.json"
RCH_DAEMON_START_LOG="${LOG_DIR}/${run_id}.rch_daemon_start.json"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\r'/\\r}
  value=${value//$'\t'/\\t}
  printf '%s' "$value"
}

json_string() {
  printf '"%s"' "$(json_escape "$1")"
}

log_json() {
  local payload="$1"
  jq -cn \
    --arg timestamp "$(now_ts)" \
    --arg component "$component" \
    --arg run_id "$run_id" \
    --arg correlation_id "$run_id" \
    --arg artifact_path "${json_log#"$ROOT_DIR"/}" \
    --argjson payload "$payload" \
    '{
      timestamp: $timestamp,
      component: $component,
      run_id: $run_id,
      scenario_id: "unspecified",
      pane_id: null,
      step: "unspecified",
      status: "running",
      correlation_id: $correlation_id,
      decision_path: "suite",
      inputs: {},
      outcome: "running",
      reason_code: null,
      error_code: null,
      artifact_path: $artifact_path
    } + $payload' >>"$json_log"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"prereq_check\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{\"command\":\"$cmd\"},\"outcome\":\"failed\",\"reason_code\":\"missing_prerequisite\",\"error_code\":\"E2E-PREREQ\"}"
    echo "missing required command: $cmd" >&2
    exit 1
  fi
}

probe_rch_workers() {
  local probe_log="$raw_dir/${run_id}.rch_probe.json"
  local probe_json

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"running\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${probe_log#"$ROOT_DIR"/}\"}"

  set +e
  env TMPDIR="$local_tmpdir" rch workers probe --all --json >"$probe_log" 2>&1
  local probe_rc=$?
  set -e

  if [[ $probe_rc -ne 0 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_probe_failed\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#"$ROOT_DIR"/}\"}"
    echo "rch workers probe failed" >&2
    exit 2
  fi

  probe_json="$(awk 'capture || /^[[:space:]]*[{]/{capture=1; print}' "$probe_log")"
  local healthy_workers
  healthy_workers="$(printf '%s\n' "$probe_json" | jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' 2>/dev/null || echo 0)"
  if [[ "$healthy_workers" -lt 1 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"failed\",\"reason_code\":\"rch_workers_unreachable\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#"$ROOT_DIR"/}\"}"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  fi

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"passed\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"artifact_path\":\"${probe_log#"$ROOT_DIR"/}\"}"
}

ensure_rch_daemon_running() {
  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_status\",\"status\":\"running\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"

  set +e
  env TMPDIR="$local_tmpdir" rch daemon status --json >"$RCH_DAEMON_STATUS_LOG" 2>&1
  local status_rc=$?
  set -e

  if [[ $status_rc -ne 0 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_status\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_daemon_status_failed\",\"error_code\":\"RCH-E101\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"
    echo "rch daemon status failed" >&2
    exit 2
  fi

  if jq -e '.data.running == true' "$RCH_DAEMON_STATUS_LOG" >/dev/null 2>&1; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_status\",\"status\":\"passed\",\"decision_path\":\"preflight\",\"inputs\":{\"running\":true},\"outcome\":\"pass\",\"reason_code\":\"daemon_running\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"
    return 0
  fi

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_status\",\"status\":\"passed\",\"decision_path\":\"preflight\",\"inputs\":{\"running\":false},\"outcome\":\"pass\",\"reason_code\":\"daemon_not_running\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"
  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_start\",\"status\":\"running\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${RCH_DAEMON_START_LOG#"$ROOT_DIR"/}\"}"

  set +e
  env TMPDIR="$local_tmpdir" rch daemon start --json >"$RCH_DAEMON_START_LOG" 2>&1
  local start_rc=$?
  set -e

  if [[ $start_rc -ne 0 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_start\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_daemon_start_failed\",\"error_code\":\"RCH-E101\",\"artifact_path\":\"${RCH_DAEMON_START_LOG#"$ROOT_DIR"/}\"}"
    echo "rch daemon start failed" >&2
    exit 2
  fi

  sleep 2

  set +e
  env TMPDIR="$local_tmpdir" rch daemon status --json >"$RCH_DAEMON_STATUS_LOG" 2>&1
  status_rc=$?
  set -e

  if [[ $status_rc -ne 0 ]] || ! jq -e '.data.running == true' "$RCH_DAEMON_STATUS_LOG" >/dev/null 2>&1; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_start\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_daemon_unavailable\",\"error_code\":\"RCH-E101\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"
    echo "rch daemon unavailable; refusing rch exec because it would fall back locally" >&2
    exit 2
  fi

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_daemon_start\",\"status\":\"passed\",\"decision_path\":\"preflight\",\"inputs\":{\"running\":true},\"outcome\":\"pass\",\"reason_code\":\"daemon_started\",\"artifact_path\":\"${RCH_DAEMON_STATUS_LOG#"$ROOT_DIR"/}\"}"
}

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

capture_rch_queue_timeout_log() {
    local output_file="$1"
    local queue_log="${output_file%.log}.rch_queue_timeout.log"
    if ! run_rch queue >"${queue_log}" 2>&1; then
        queue_log="${output_file}"
    fi
    printf '%s\n' "${queue_log}"
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
        local queue_log
        queue_log="$(capture_rch_queue_timeout_log "${output_file}")"
        fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${queue_log}"
    fi
    return "${rc}"
}

ensure_rch_ready() {
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi
    ensure_rch_daemon_running
}

extract_section_json_line() {
  local file="$1"
  local marker="$2"
  awk -v marker="$marker" '$0 == marker { getline; print; exit }' "$file"
}

compute_timeline_sha() {
  local file="$1"
  awk '
    $0 == "--- ftreplay-timeline ---" { in_timeline=1; next }
    /^--- ftreplay-/ { if (in_timeline) { exit } }
    in_timeline { print }
  ' "$file" | shasum -a 256 | awk '{print $1}'
}

run_harvest_command() {
  local source_dir="$1"
  local output_dir="$2"
  local filter="$3"
  local stdout_file="$4"
  local stderr_file="$5"
  local combined_file="${stdout_file%.json}.combined.log"

  # Convert absolute local paths to relative paths so they resolve correctly
  # on the remote worker where the project root differs from the local machine.
  local rel_source_dir="${source_dir#"$ROOT_DIR"/}"
  local rel_output_dir="${output_dir#"$ROOT_DIR"/}"

  set +e
  (
    cd "${ROOT_DIR}"
    env TMPDIR="$local_tmpdir" "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
      rch exec -- env \
      TMPDIR="$remote_tmpdir" \
      CARGO_HOME="$cargo_home" \
      CARGO_TARGET_DIR="$cargo_target_dir" \
      cargo run -q -p frankenterm -- \
      replay harvest \
      --source-dir "$rel_source_dir" \
      --output-dir "$rel_output_dir" \
      --filter "$filter" \
      --json
  ) >"$combined_file" 2>&1
  local rc=$?
  set -e

  # Copy combined output for downstream consumers that expect separate files
  cp "$combined_file" "$stdout_file" 2>/dev/null || true
  cp "$combined_file" "$stderr_file" 2>/dev/null || true

  check_rch_fallback "$combined_file"

  if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
    local queue_log
    queue_log="$(capture_rch_queue_timeout_log "${combined_file}")"
    fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${queue_log}"
  fi

  return "$rc"
}

emit_event_line() {
  local file="$1"
  local event_id="$2"
  local pane_id="$3"
  local sequence="$4"
  local text="$5"

  printf '{"schema_version":"ft.recorder.event.v1","event_id":%s,"pane_id":%s,"session_id":%s,"workflow_id":null,"correlation_id":null,"source":"wezterm_mux","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"egress_output","text":%s,"encoding":"utf8","redaction":"none","segment_kind":"delta","is_gap":false}\n' \
    "$(json_string "$event_id")" \
    "$pane_id" \
    "$(json_string "sess-$pane_id")" \
    $((1700000000000 + sequence)) \
    $((1700000000000 + sequence)) \
    "$sequence" \
    "$(json_string "$text")" >>"$file"
}

emit_decision_line() {
  local file="$1"
  local event_id="$2"
  local sequence="$3"

  printf '{"schema_version":"ft.recorder.event.v1","event_id":%s,"pane_id":1,"session_id":"sess-incident","workflow_id":"wf-1","correlation_id":"corr-1","source":"workflow_engine","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"control_marker","control_marker_type":"policy_decision","details":{"decision":"allow","reason":"fixture","rule_id":"policy.default.allow_non_alt","action_kind":"send_text"}}\n' \
    "$(json_string "$event_id")" \
    $((1700000010000 + sequence)) \
    $((1700000010000 + sequence)) \
    "$sequence" >>"$file"
}

write_fixture() {
  local file="$1"
  local event_count="$2"
  local include_decision="$3"
  local id_prefix="$4"

  : >"$file"
  for ((i = 0; i < event_count; i++)); do
    local pane_id=$(( (i % 2) + 1 ))
    emit_event_line "$file" "${id_prefix}-ev-${i}" "$pane_id" "$i" "${id_prefix}-line-${i}"
  done

  if [[ "$include_decision" == "yes" ]]; then
    emit_decision_line "$file" "${id_prefix}-ev-decision" "$event_count"
  fi
}

require_cmd jq
require_cmd rch
require_cmd cargo
require_cmd shasum
probe_rch_workers
ensure_rch_ready

log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"

# Pre-compile the frankenterm binary on the remote worker to avoid SSH timeout
# during the first cargo run invocation. The binary is large and cold compiles
# exceed rch's 300s SSH timeout.
precompile_log="$raw_dir/precompile.combined.log"
log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"precompile\",\"status\":\"running\",\"decision_path\":\"precompile\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${precompile_log#"$ROOT_DIR"/}\"}"
set +e
run_rch_cargo_logged "$precompile_log" env \
  TMPDIR="$remote_tmpdir" \
  CARGO_HOME="$cargo_home" \
  CARGO_TARGET_DIR="$cargo_target_dir" \
  cargo build -q -p frankenterm
precompile_rc=$?
set -e
if [[ $precompile_rc -ne 0 ]]; then
  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"precompile\",\"status\":\"failed\",\"decision_path\":\"precompile\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"precompile_failed\",\"error_code\":\"$precompile_rc\",\"artifact_path\":\"${precompile_log#"$ROOT_DIR"/}\"}"
  tail -n 30 "$precompile_log" >&2 || true
  exit 1
fi
log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"precompile\",\"status\":\"passed\",\"decision_path\":\"precompile\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"binary_cached\",\"artifact_path\":\"${precompile_log#"$ROOT_DIR"/}\"}"

# Run all 4 artifact write/read scenarios as integration tests via cargo test.
# This approach is rch-compatible: tests create their own fixtures in tempdir
# (no gitignored paths), produce output on the remote worker, and validate
# inline. The integration tests in replay_capture_integration.rs cover:
#   1. Artifact section structure + integrity SHA (replay_capture_artifact_sections_and_integrity_check)
#   2. Tamper detection via timeline modification (replay_capture_tamper_detection_catches_modified_timeline)
#   3. Recovery path via re-harvest (replay_capture_recovery_reharvest_produces_valid_artifact)
#   4. Chunked output with manifest (replay_capture_chunked_artifact_with_manifest)
test_filter="replay_capture_artifact_sections_and_integrity_check\|replay_capture_tamper_detection\|replay_capture_recovery_reharvest\|replay_capture_chunked_artifact"
cargo_test_log="$raw_dir/integration_tests.combined.log"

log_json "{\"scenario_id\":\"all\",\"step\":\"cargo_test\",\"status\":\"running\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test_filter\":\"$test_filter\"},\"outcome\":\"running\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"

set +e
run_rch_cargo_logged "$cargo_test_log" env \
  TMPDIR="$remote_tmpdir" \
  CARGO_HOME="$cargo_home" \
  CARGO_TARGET_DIR="$cargo_target_dir" \
  cargo test -p frankenterm-core --test replay_capture_integration "$test_filter" -- --nocapture
rc=$?
set -e

if [[ $rc -ne 0 ]]; then
  log_json "{\"scenario_id\":\"all\",\"step\":\"cargo_test\",\"status\":\"failed\",\"decision_path\":\"cargo_test\",\"inputs\":{\"test_filter\":\"$test_filter\",\"error_context\":\"integration tests failed\"},\"outcome\":\"failed\",\"reason_code\":\"integration_test_failed\",\"error_code\":\"$rc\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"
  tail -n 120 "$cargo_test_log" >&2 || true
  exit 1
fi

# Verify non-zero tests actually ran
if grep -Eq 'running 0 tests|0 passed; 0 failed' "$cargo_test_log" 2>/dev/null; then
  log_json "{\"scenario_id\":\"all\",\"step\":\"cargo_test\",\"status\":\"failed\",\"decision_path\":\"cargo_test\",\"inputs\":{\"error_context\":\"zero tests matched filter\"},\"outcome\":\"failed\",\"reason_code\":\"zero_tests_ran\",\"error_code\":\"ZERO-TESTS\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"
  exit 1
fi

log_json "{\"scenario_id\":\"1\",\"step\":\"validate_sections_and_integrity\",\"status\":\"passed\",\"decision_path\":\"scenario_1.validate_sections_and_integrity\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"
log_json "{\"scenario_id\":\"2\",\"step\":\"tamper_detection\",\"status\":\"passed\",\"decision_path\":\"scenario_2.tamper_detection\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"tamper_detected\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"
log_json "{\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"passed\",\"decision_path\":\"scenario_3.recovery\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"recovery_integrity_verified\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"
log_json "{\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"passed\",\"decision_path\":\"scenario_4.chunking\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"chunk_manifest_valid\",\"artifact_path\":\"${cargo_test_log#"$ROOT_DIR"/}\"}"

log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"passed\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"all_checks_passed\",\"artifact_path\":\"${json_log#"$ROOT_DIR"/}\"}"
echo "Replay artifact write/read e2e passed. Logs: ${json_log#"$ROOT_DIR"/}"
