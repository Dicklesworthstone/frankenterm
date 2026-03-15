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
cargo_target_dir="${FT_REPLAY_CAPTURE_TARGET_DIR:-$ROOT_DIR/target-replay-artifact-write-read-${run_id}}"
local_tmpdir="${FT_REPLAY_CAPTURE_LOCAL_TMPDIR:-${TMPDIR:-/tmp}}"
remote_tmpdir="${FT_REPLAY_CAPTURE_REMOTE_TMPDIR:-/home/ubuntu}"
work_dir="$ROOT_DIR/tests/e2e/tmp/${run_id}"
mkdir -p "$work_dir"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_artifact_write_read_${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_artifact_write_read_${run_id}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  local payload="$1"
  jq -cn \
    --arg timestamp "$(now_ts)" \
    --arg component "$component" \
    --arg run_id "$run_id" \
    --arg correlation_id "$run_id" \
    --arg artifact_path "${json_log#$ROOT_DIR/}" \
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

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"running\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"

  set +e
  env TMPDIR="$local_tmpdir" rch workers probe --all --json >"$probe_log" 2>&1
  local probe_rc=$?
  set -e

  if [[ $probe_rc -ne 0 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_probe_failed\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "rch workers probe failed" >&2
    exit 2
  fi

  probe_json="$(awk 'capture || /^[[:space:]]*[{]/{capture=1; print}' "$probe_log")"
  local healthy_workers
  healthy_workers="$(printf '%s\n' "$probe_json" | jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' 2>/dev/null || echo 0)"
  if [[ "$healthy_workers" -lt 1 ]]; then
    log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"failed\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"failed\",\"reason_code\":\"rch_workers_unreachable\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  fi

  log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"rch_probe\",\"status\":\"passed\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
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
      --source-dir "$source_dir" \
      --output-dir "$output_dir" \
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
    fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s. See ${combined_file}"
  fi

  if grep -Eq '\[RCH\][[:space:]]+local|fail-open' "$combined_file"; then
    return 97
  fi

  return "$rc"
}

emit_event_line() {
  local file="$1"
  local event_id="$2"
  local pane_id="$3"
  local sequence="$4"
  local text="$5"

  printf '{"schema_version":"ft.recorder.event.v1","event_id":"%s","pane_id":%s,"session_id":"sess-%s","workflow_id":null,"correlation_id":null,"source":"wezterm_mux","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"egress_output","text":"%s","encoding":"utf8","redaction":"none","segment_kind":"delta","is_gap":false}\n' \
    "$event_id" "$pane_id" "$pane_id" $((1700000000000 + sequence)) $((1700000000000 + sequence)) "$sequence" "$text" >>"$file"
}

emit_decision_line() {
  local file="$1"
  local event_id="$2"
  local sequence="$3"

  printf '{"schema_version":"ft.recorder.event.v1","event_id":"%s","pane_id":1,"session_id":"sess-incident","workflow_id":"wf-1","correlation_id":"corr-1","source":"workflow_engine","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"control_marker","control_marker_type":"policy_decision","details":{"decision":"allow","reason":"fixture","rule_id":"policy.default.allow_non_alt","action_kind":"send_text"}}\n' \
    "$event_id" $((1700000010000 + sequence)) $((1700000010000 + sequence)) "$sequence" >>"$file"
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

log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

# Scenario 1: basic write + read integrity checks
scenario1_src="$work_dir/scenario1/incidents"
scenario1_out="$work_dir/scenario1/out"
mkdir -p "$scenario1_src" "$scenario1_out"
write_fixture "$scenario1_src/incident_base.jsonl" 120 yes "base"

scenario1_stdout="$raw_dir/scenario1.stdout.json"
scenario1_stderr="$raw_dir/scenario1.stderr.log"
log_json "{\"scenario_id\":\"1\",\"step\":\"run_harvest\",\"status\":\"running\",\"decision_path\":\"scenario_1.harvest\",\"inputs\":{\"source_dir\":\"${scenario1_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario1_out#$ROOT_DIR/}\",\"filter\":\"incident-only\"},\"outcome\":\"running\",\"artifact_path\":\"${scenario1_stdout#$ROOT_DIR/}\"}"
set +e
run_harvest_command "$scenario1_src" "$scenario1_out" "incident-only" "$scenario1_stdout" "$scenario1_stderr"
rc=$?
set -e
if [[ $rc -ne 0 ]]; then
  if [[ $rc -eq 97 ]]; then
    log_json "{\"scenario_id\":\"1\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_1.harvest\",\"inputs\":{\"source_dir\":\"${scenario1_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario1_out#$ROOT_DIR/}\",\"filter\":\"incident-only\",\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${scenario1_stderr#$ROOT_DIR/}\"}"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
  else
    log_json "{\"scenario_id\":\"1\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_1.harvest\",\"inputs\":{\"source_dir\":\"${scenario1_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario1_out#$ROOT_DIR/}\",\"filter\":\"incident-only\",\"error_context\":\"harvest command failed\"},\"outcome\":\"failed\",\"reason_code\":\"harvest_command_failed\",\"error_code\":\"$rc\",\"artifact_path\":\"${scenario1_stderr#$ROOT_DIR/}\"}"
    tail -n 120 "$scenario1_stderr" >&2 || true
  fi
  exit 1
fi

scenario1_harvested=$(jq -r '.harvested' "$scenario1_stdout")
if [[ "$scenario1_harvested" -ne 1 ]]; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"assert_harvested\",\"status\":\"failed\",\"decision_path\":\"scenario_1.assert_harvested\",\"inputs\":{\"harvested\":$scenario1_harvested,\"error_context\":\"expected one harvested artifact\"},\"outcome\":\"failed\",\"reason_code\":\"unexpected_harvest_count\",\"error_code\":\"unexpected_harvest_count\",\"artifact_path\":\"${scenario1_stdout#$ROOT_DIR/}\"}"
  tail -n 120 "$scenario1_stderr" >&2 || true
  exit 1
fi

artifact_path="$(find "$scenario1_out" -type f -name '*.ftreplay' | head -n 1)"
if [[ -z "$artifact_path" || ! -f "$artifact_path" ]]; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"locate_artifact\",\"status\":\"failed\",\"decision_path\":\"scenario_1.locate_artifact\",\"inputs\":{\"error_context\":\"artifact file not found\"},\"outcome\":\"failed\",\"reason_code\":\"artifact_missing\",\"error_code\":\"artifact_missing\"}"
  exit 1
fi

header_json="$(extract_section_json_line "$artifact_path" "--- ftreplay-header ---")"
footer_json="$(extract_section_json_line "$artifact_path" "--- ftreplay-footer ---")"
if [[ -z "$header_json" || -z "$footer_json" ]]; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"validate_sections\",\"status\":\"failed\",\"decision_path\":\"scenario_1.validate_sections\",\"inputs\":{\"error_context\":\"missing header/footer sections\"},\"outcome\":\"failed\",\"reason_code\":\"missing_required_sections\",\"error_code\":\"missing_required_sections\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

expected_timeline_sha="$(jq -r '.integrity.timeline_sha256' <<<"$header_json")"
actual_timeline_sha="$(compute_timeline_sha "$artifact_path")"
if [[ "$expected_timeline_sha" != "$actual_timeline_sha" ]]; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"validate_integrity\",\"status\":\"failed\",\"decision_path\":\"scenario_1.validate_integrity\",\"inputs\":{\"expected\":\"$expected_timeline_sha\",\"actual\":\"$actual_timeline_sha\"},\"outcome\":\"failed\",\"reason_code\":\"timeline_sha_mismatch\",\"error_code\":\"timeline_sha_mismatch\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

if ! jq -e '.schema_version == "ftreplay.v1" and (.content.event_count >= 100) and (.content.decision_count >= 1)' <<<"$header_json" >/dev/null; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"validate_header\",\"status\":\"failed\",\"decision_path\":\"scenario_1.validate_header\",\"inputs\":{\"error_context\":\"header schema/content assertion failed\"},\"outcome\":\"failed\",\"reason_code\":\"invalid_header_schema\",\"error_code\":\"invalid_header_schema\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

if ! jq -e '.schema_version == "ftreplay.v1" and (.event_count_verified >= 100) and (.integrity_check.timeline_sha256_match == true)' <<<"$footer_json" >/dev/null; then
  log_json "{\"scenario_id\":\"1\",\"step\":\"validate_footer\",\"status\":\"failed\",\"decision_path\":\"scenario_1.validate_footer\",\"inputs\":{\"error_context\":\"footer schema/content assertion failed\"},\"outcome\":\"failed\",\"reason_code\":\"invalid_footer_schema\",\"error_code\":\"invalid_footer_schema\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

log_json "{\"scenario_id\":\"1\",\"step\":\"validate_sections_and_integrity\",\"status\":\"passed\",\"decision_path\":\"scenario_1.validate_sections_and_integrity\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"assertions_satisfied\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"

# Scenario 2: failure injection by tampering timeline content
tampered_path="$work_dir/scenario2_tampered.ftreplay"
awk '
  $0 == "--- ftreplay-timeline ---" { print; in_timeline = 1; next }
  in_timeline && !tampered && $0 !~ /^--- ftreplay-/ {
    sub(/"text":"[^"]+"/, "\"text\":\"tampered-line\"")
    tampered = 1
  }
  { print }
' "$artifact_path" >"$tampered_path"

tampered_header="$(extract_section_json_line "$tampered_path" "--- ftreplay-header ---")"
tampered_expected_sha="$(jq -r '.integrity.timeline_sha256' <<<"$tampered_header")"
tampered_actual_sha="$(compute_timeline_sha "$tampered_path")"
if [[ "$tampered_expected_sha" == "$tampered_actual_sha" ]]; then
  log_json "{\"scenario_id\":\"2\",\"step\":\"tamper_detection\",\"status\":\"failed\",\"decision_path\":\"scenario_2.tamper_detection\",\"inputs\":{\"error_context\":\"tampering did not alter digest\"},\"outcome\":\"failed\",\"reason_code\":\"tamper_not_detected\",\"error_code\":\"tamper_not_detected\",\"artifact_path\":\"${tampered_path#$ROOT_DIR/}\"}"
  exit 1
fi
log_json "{\"scenario_id\":\"2\",\"step\":\"tamper_detection\",\"status\":\"passed\",\"decision_path\":\"scenario_2.tamper_detection\",\"inputs\":{\"expected\":\"$tampered_expected_sha\",\"actual\":\"$tampered_actual_sha\"},\"outcome\":\"pass\",\"reason_code\":\"tamper_detected\",\"artifact_path\":\"${tampered_path#$ROOT_DIR/}\"}"

# Scenario 3: recovery path (fresh output rerun restores valid artifact)
scenario3_out="$work_dir/scenario3_recovery/out"
mkdir -p "$scenario3_out"
scenario3_stdout="$raw_dir/scenario3.stdout.json"
scenario3_stderr="$raw_dir/scenario3.stderr.log"
set +e
run_harvest_command "$scenario1_src" "$scenario3_out" "incident-only" "$scenario3_stdout" "$scenario3_stderr"
rc=$?
set -e
if [[ $rc -ne 0 ]]; then
  if [[ $rc -eq 97 ]]; then
    log_json "{\"scenario_id\":\"3\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_3.recovery_harvest\",\"inputs\":{\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${scenario3_stderr#$ROOT_DIR/}\"}"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
  else
    log_json "{\"scenario_id\":\"3\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_3.recovery_harvest\",\"inputs\":{\"error_context\":\"harvest command failed\"},\"outcome\":\"failed\",\"reason_code\":\"harvest_command_failed\",\"error_code\":\"$rc\",\"artifact_path\":\"${scenario3_stderr#$ROOT_DIR/}\"}"
    tail -n 120 "$scenario3_stderr" >&2 || true
  fi
  exit 1
fi

recovered_artifact="$(find "$scenario3_out" -type f -name '*.ftreplay' | head -n 1)"
if [[ -z "$recovered_artifact" || ! -f "$recovered_artifact" ]]; then
  log_json "{\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"failed\",\"decision_path\":\"scenario_3.recovery\",\"inputs\":{\"error_context\":\"recovered artifact missing\"},\"outcome\":\"failed\",\"reason_code\":\"recovered_artifact_missing\",\"error_code\":\"recovered_artifact_missing\"}"
  exit 1
fi

recovered_header="$(extract_section_json_line "$recovered_artifact" "--- ftreplay-header ---")"
recovered_expected_sha="$(jq -r '.integrity.timeline_sha256' <<<"$recovered_header")"
recovered_actual_sha="$(compute_timeline_sha "$recovered_artifact")"
if [[ "$recovered_expected_sha" != "$recovered_actual_sha" ]]; then
  log_json "{\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"failed\",\"decision_path\":\"scenario_3.recovery\",\"inputs\":{\"error_context\":\"recovered artifact integrity mismatch\"},\"outcome\":\"failed\",\"reason_code\":\"recovery_integrity_mismatch\",\"error_code\":\"recovery_integrity_mismatch\",\"artifact_path\":\"${recovered_artifact#$ROOT_DIR/}\"}"
  exit 1
fi
log_json "{\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"passed\",\"decision_path\":\"scenario_3.recovery\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"recovery_integrity_verified\",\"artifact_path\":\"${recovered_artifact#$ROOT_DIR/}\"}"

# Scenario 4: chunking path (>100k events)
scenario4_src="$work_dir/scenario4/incidents"
scenario4_out="$work_dir/scenario4/out"
mkdir -p "$scenario4_src" "$scenario4_out"
write_fixture "$scenario4_src/incident_large.jsonl" 100020 yes "large"

scenario4_stdout="$raw_dir/scenario4.stdout.json"
scenario4_stderr="$raw_dir/scenario4.stderr.log"
set +e
run_harvest_command "$scenario4_src" "$scenario4_out" "incident-only" "$scenario4_stdout" "$scenario4_stderr"
rc=$?
set -e
if [[ $rc -ne 0 ]]; then
  if [[ $rc -eq 97 ]]; then
    log_json "{\"scenario_id\":\"4\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_4.chunk_harvest\",\"inputs\":{\"error_context\":\"rch local fallback detected\"},\"outcome\":\"failed\",\"reason_code\":\"rch_fail_open_local_fallback\",\"error_code\":\"RCH-LOCAL-FALLBACK\",\"artifact_path\":\"${scenario4_stderr#$ROOT_DIR/}\"}"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
  else
    log_json "{\"scenario_id\":\"4\",\"step\":\"run_harvest\",\"status\":\"failed\",\"decision_path\":\"scenario_4.chunk_harvest\",\"inputs\":{\"error_context\":\"harvest command failed\"},\"outcome\":\"failed\",\"reason_code\":\"harvest_command_failed\",\"error_code\":\"$rc\",\"artifact_path\":\"${scenario4_stderr#$ROOT_DIR/}\"}"
    tail -n 120 "$scenario4_stderr" >&2 || true
  fi
  exit 1
fi

manifest_path="$(find "$scenario4_out" -type f -name '*.manifest.json' | head -n 1)"
if [[ -z "$manifest_path" || ! -f "$manifest_path" ]]; then
  log_json "{\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"failed\",\"decision_path\":\"scenario_4.chunking\",\"inputs\":{\"error_context\":\"manifest file missing\"},\"outcome\":\"failed\",\"reason_code\":\"manifest_missing\",\"error_code\":\"manifest_missing\"}"
  exit 1
fi

chunk_count="$(jq -r '.chunk_count' "$manifest_path")"
if [[ "$chunk_count" -lt 2 ]]; then
  log_json "{\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"failed\",\"decision_path\":\"scenario_4.chunking\",\"inputs\":{\"chunk_count\":$chunk_count},\"outcome\":\"failed\",\"reason_code\":\"chunk_count_too_small\",\"error_code\":\"chunk_count_too_small\",\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"
  exit 1
fi

missing_chunk=0
while IFS= read -r chunk_rel; do
  if [[ ! -f "$scenario4_out/$chunk_rel" ]]; then
    missing_chunk=1
    break
  fi
done < <(jq -r '.chunks[].path' "$manifest_path")
if [[ "$missing_chunk" -ne 0 ]]; then
  log_json "{\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"failed\",\"decision_path\":\"scenario_4.chunking\",\"inputs\":{\"error_context\":\"manifest references missing chunk\"},\"outcome\":\"failed\",\"reason_code\":\"chunk_file_missing\",\"error_code\":\"chunk_file_missing\",\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"
  exit 1
fi

log_json "{\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"passed\",\"decision_path\":\"scenario_4.chunking\",\"inputs\":{\"chunk_count\":$chunk_count},\"outcome\":\"pass\",\"reason_code\":\"chunk_manifest_valid\",\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"

log_json "{\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"passed\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"pass\",\"reason_code\":\"all_checks_passed\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
echo "Replay artifact write/read e2e passed. Logs: ${json_log#$ROOT_DIR/}"
