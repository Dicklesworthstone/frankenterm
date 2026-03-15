#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_fixture_harvest_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="fixture_harvest_pipeline"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="/tmp/cargo-home-replay-fixture-harvest"
cargo_target_dir="$ROOT_DIR/target-replay-fixture-harvest-${run_id}"
work_dir="$ROOT_DIR/tests/e2e/tmp/${run_id}"
mkdir -p "$work_dir"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_fixture_harvest_${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_fixture_harvest_${run_id}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
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

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
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
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this E2E harness; refusing local cargo execution."
    fi
    resolve_timeout_bin
    if [[ -z "${TIMEOUT_BIN}" ]]; then
        fatal "timeout or gtimeout is required to fail closed on stalled remote execution."
    fi
    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
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

run_harvest_command() {
  local source_dir="$1"
  local output_dir="$2"
  local filter="$3"
  local stdout_file="$4"
  local stderr_file="$5"
  local combined_file="${stdout_file%.json}.combined.log"

  set +e
  run_rch_cargo_logged "$combined_file" env \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo run -q -p frankenterm -- \
    replay harvest \
    --source-dir "$source_dir" \
    --output-dir "$output_dir" \
    --filter "$filter" \
    --json
  local rc=$?
  set -e

  # Extract stdout/stderr from combined log for downstream consumers
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

  printf '{"schema_version":"ft.recorder.event.v1","event_id":"%s","pane_id":%s,"session_id":"sess-%s","workflow_id":null,"correlation_id":null,"source":"wezterm_mux","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"egress_output","text":"%s","encoding":"utf8","redaction":"none","segment_kind":"delta","is_gap":false}\n' \
    "$event_id" "$pane_id" "$pane_id" $((1700000000000 + sequence)) $((1700000000000 + sequence)) "$sequence" "$text" >>"$file"
}

emit_decision_line() {
  local file="$1"
  local event_id="$2"
  local sequence="$3"

  printf '{"schema_version":"ft.recorder.event.v1","event_id":"%s","pane_id":1,"session_id":"sess-incident","workflow_id":"wf-1","correlation_id":"corr-1","source":"workflow_engine","occurred_at_ms":%s,"recorded_at_ms":%s,"sequence":%s,"causality":{"parent_event_id":null,"trigger_event_id":null,"root_event_id":null},"event_type":"control_marker","control_marker_type":"policy_decision","details":{"decision":"allow","reason":"fixture"}}\n' \
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

ensure_rch_ready

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"run_id\":\"$run_id\"}"

# Scenario 1: harvest three valid incident recordings
scenario1_src="$work_dir/scenario1/incidents"
scenario1_out="$work_dir/scenario1/out"
mkdir -p "$scenario1_src" "$scenario1_out"
write_fixture "$scenario1_src/incident_alpha.jsonl" 120 yes "alpha"
write_fixture "$scenario1_src/incident_bravo.jsonl" 125 yes "bravo"
write_fixture "$scenario1_src/incident_charlie.jsonl" 130 yes "charlie"

scenario1_stdout="$raw_dir/scenario1.stdout.json"
scenario1_stderr="$raw_dir/scenario1.stderr.log"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"1\",\"step\":\"run_harvest\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"source_dir\":\"${scenario1_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario1_out#$ROOT_DIR/}\",\"filter\":\"incident-only\"},\"decision_path\":\"incident_only\"}"
run_harvest_command "$scenario1_src" "$scenario1_out" "incident-only" "$scenario1_stdout" "$scenario1_stderr"

scenario1_harvested=$(jq -r '.harvested' "$scenario1_stdout")
if [[ "$scenario1_harvested" -ne 3 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"1\",\"step\":\"assert_harvested_count\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"expected 3 harvested\",\"inputs\":{\"harvested\":$scenario1_harvested},\"error_code\":\"unexpected_harvest_count\",\"artifact_path\":\"${scenario1_stdout#$ROOT_DIR/}\"}"
  tail -n 120 "$scenario1_stderr" >&2 || true
  exit 1
fi
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"1\",\"step\":\"assert_harvested_count\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"outcome\":\"3 fixtures harvested\",\"inputs\":{\"harvested\":$scenario1_harvested},\"artifact_path\":\"${scenario1_stdout#$ROOT_DIR/}\"}"

# Scenario 2: quality filter skips undersized artifact (<100 events)
scenario2_src="$work_dir/scenario2/incidents"
scenario2_out="$work_dir/scenario2/out"
mkdir -p "$scenario2_src" "$scenario2_out"
write_fixture "$scenario2_src/incident_tiny.jsonl" 12 yes "tiny"

scenario2_stdout="$raw_dir/scenario2.stdout.json"
scenario2_stderr="$raw_dir/scenario2.stderr.log"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"2\",\"step\":\"run_harvest\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"source_dir\":\"${scenario2_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario2_out#$ROOT_DIR/}\",\"filter\":\"incident-only\"},\"decision_path\":\"quality_filters\"}"
run_harvest_command "$scenario2_src" "$scenario2_out" "incident-only" "$scenario2_stdout" "$scenario2_stderr"

scenario2_skipped=$(jq -r '.skipped' "$scenario2_stdout")
if [[ "$scenario2_skipped" -lt 1 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"2\",\"step\":\"assert_quality_skip\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"expected at least one skipped artifact\",\"inputs\":{\"skipped\":$scenario2_skipped},\"error_code\":\"quality_filter_missed\",\"artifact_path\":\"${scenario2_stdout#$ROOT_DIR/}\"}"
  tail -n 120 "$scenario2_stderr" >&2 || true
  exit 1
fi
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"2\",\"step\":\"assert_quality_skip\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"outcome\":\"undersized fixture skipped\",\"inputs\":{\"skipped\":$scenario2_skipped},\"artifact_path\":\"${scenario2_stdout#$ROOT_DIR/}\"}"

# Scenario 3: duplicate overlap detection skips second near-identical artifact
scenario3_src="$work_dir/scenario3/incidents"
scenario3_out="$work_dir/scenario3/out"
mkdir -p "$scenario3_src" "$scenario3_out"
write_fixture "$scenario3_src/incident_dup_a.jsonl" 120 yes "dup"
write_fixture "$scenario3_src/incident_dup_b.jsonl" 120 yes "dup"

scenario3_stdout="$raw_dir/scenario3.stdout.json"
scenario3_stderr="$raw_dir/scenario3.stderr.log"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"3\",\"step\":\"run_harvest\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"source_dir\":\"${scenario3_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario3_out#$ROOT_DIR/}\",\"filter\":\"incident-only\"},\"decision_path\":\"duplicate_detection\"}"
run_harvest_command "$scenario3_src" "$scenario3_out" "incident-only" "$scenario3_stdout" "$scenario3_stderr"

scenario3_harvested=$(jq -r '.harvested' "$scenario3_stdout")
scenario3_skipped=$(jq -r '.skipped' "$scenario3_stdout")
if [[ "$scenario3_harvested" -lt 1 || "$scenario3_skipped" -lt 1 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"3\",\"step\":\"assert_duplicate_skip\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"expected one harvest and one duplicate skip\",\"inputs\":{\"harvested\":$scenario3_harvested,\"skipped\":$scenario3_skipped},\"error_code\":\"duplicate_not_detected\",\"artifact_path\":\"${scenario3_stdout#$ROOT_DIR/}\"}"
  tail -n 120 "$scenario3_stderr" >&2 || true
  exit 1
fi
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"3\",\"step\":\"assert_duplicate_skip\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"outcome\":\"duplicate overlap skipped\",\"inputs\":{\"harvested\":$scenario3_harvested,\"skipped\":$scenario3_skipped},\"artifact_path\":\"${scenario3_stdout#$ROOT_DIR/}\"}"

# Scenario 4: verify harvested artifacts pass schema checks and registry tracks entries
registry_path="$scenario1_out/fixture_registry.json"
artifact_count=$(find "$scenario1_out" -type f -name '*.ftreplay' | wc -l | tr -d ' ')
registry_size=$(jq -r '.artifacts | length' "$registry_path")

if [[ "$artifact_count" -lt 3 || "$registry_size" -lt 3 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"4\",\"step\":\"assert_registry_and_schema\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"expected >=3 harvested artifacts and registry entries\",\"inputs\":{\"artifact_count\":$artifact_count,\"registry_size\":$registry_size},\"error_code\":\"artifact_registry_mismatch\",\"artifact_path\":\"${registry_path#$ROOT_DIR/}\"}"
  exit 1
fi

schema_fail=0
while IFS= read -r artifact; do
  header_json="$(extract_section_json_line "$artifact" "--- ftreplay-header ---")"
  footer_json="$(extract_section_json_line "$artifact" "--- ftreplay-footer ---")"

  if [[ -z "$header_json" || -z "$footer_json" ]]; then
    schema_fail=1
    break
  fi

  if ! jq -e '.schema_version == "ftreplay.v1" and (.content.event_count >= 100) and (.content.decision_count >= 1) and (.integrity.timeline_sha256 | type == "string")' <<<"$header_json" >/dev/null; then
    schema_fail=1
    break
  fi

  if ! jq -e '.schema_version == "ftreplay.v1" and (.event_count_verified >= 100) and (.decision_count_verified >= 1) and (.integrity_check.timeline_sha256_match == true)' <<<"$footer_json" >/dev/null; then
    schema_fail=1
    break
  fi
done < <(find "$scenario1_out" -type f -name '*.ftreplay' | sort)

if [[ "$schema_fail" -ne 0 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"4\",\"step\":\"assert_registry_and_schema\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"schema validation failed for one or more artifacts\",\"error_code\":\"schema_validation_failed\",\"artifact_path\":\"${scenario1_out#$ROOT_DIR/}\"}"
  exit 1
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"4\",\"step\":\"assert_registry_and_schema\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"outcome\":\"artifacts and registry validated\",\"inputs\":{\"artifact_count\":$artifact_count,\"registry_size\":$registry_size},\"artifact_path\":\"${scenario1_out#$ROOT_DIR/}\"}"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_fixture_harvest\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"pass\",\"run_id\":\"$run_id\"}"
echo "Replay fixture harvest e2e passed. Logs: ${json_log#$ROOT_DIR/}"
