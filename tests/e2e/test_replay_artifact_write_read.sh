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
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/tmp}"
default_cargo_target_dir="target/rch-e2e-replay-artifact-write-read-${run_id}"
requested_cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-}"
if [[ -n "${requested_cargo_target_dir}" && "${requested_cargo_target_dir}" != /* ]]; then
  cargo_target_dir="${requested_cargo_target_dir}"
else
  cargo_target_dir="${default_cargo_target_dir}"
fi
work_dir="$ROOT_DIR/tests/e2e/tmp/${run_id}"
mkdir -p "$work_dir"

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
RCH_LOCAL_TMPDIR="${local_tmpdir}"
GUARD_LIB="${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${raw_dir}" "${run_id}" "${component}" "${ROOT_DIR}"

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

fatal() { rch_fatal "$1"; }

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
  run_rch_cargo_logged "$combined_file" env \
    TMPDIR="$remote_tmpdir" \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo run -q -p frankenterm -- \
    replay harvest \
    --source-dir "$rel_source_dir" \
    --output-dir "$rel_output_dir" \
    --filter "$filter" \
    --json
  local rc=$?
  set -e

  # Copy combined output for downstream consumers that expect separate files
  cp "$combined_file" "$stdout_file" 2>/dev/null || true
  cp "$combined_file" "$stderr_file" 2>/dev/null || true

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
