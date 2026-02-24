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

cargo_home="/tmp/cargo-home-replay-artifact-write-read"
cargo_target_dir="$ROOT_DIR/target-replay-artifact-write-read-${run_id}"
work_dir="$ROOT_DIR/tests/e2e/tmp/${run_id}"
mkdir -p "$work_dir"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  local payload="$1"
  echo "$payload" >>"$json_log"
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

  rch exec -- env \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$cargo_target_dir" \
    cargo run -q -p frankenterm -- \
    replay harvest \
    --source-dir "$source_dir" \
    --output-dir "$output_dir" \
    --filter "$filter" \
    --json >"$stdout_file" 2>"$stderr_file"
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

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"$scenario_id\",\"step\":\"start\",\"status\":\"running\",\"run_id\":\"$run_id\"}"

# Scenario 1: basic write + read integrity checks
scenario1_src="$work_dir/scenario1/incidents"
scenario1_out="$work_dir/scenario1/out"
mkdir -p "$scenario1_src" "$scenario1_out"
write_fixture "$scenario1_src/incident_base.jsonl" 120 yes "base"

scenario1_stdout="$raw_dir/scenario1.stdout.json"
scenario1_stderr="$raw_dir/scenario1.stderr.log"
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"run_harvest\",\"status\":\"running\",\"run_id\":\"$run_id\",\"inputs\":{\"source_dir\":\"${scenario1_src#$ROOT_DIR/}\",\"output_dir\":\"${scenario1_out#$ROOT_DIR/}\",\"filter\":\"incident-only\"}}"
run_harvest_command "$scenario1_src" "$scenario1_out" "incident-only" "$scenario1_stdout" "$scenario1_stderr"

scenario1_harvested=$(jq -r '.harvested' "$scenario1_stdout")
if [[ "$scenario1_harvested" -ne 1 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"assert_harvested\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"outcome\":\"expected one harvested artifact\",\"inputs\":{\"harvested\":$scenario1_harvested},\"error_code\":\"unexpected_harvest_count\"}"
  tail -n 120 "$scenario1_stderr" >&2 || true
  exit 1
fi

artifact_path="$(find "$scenario1_out" -type f -name '*.ftreplay' | head -n 1)"
if [[ -z "$artifact_path" || ! -f "$artifact_path" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"locate_artifact\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"artifact_missing\"}"
  exit 1
fi

header_json="$(extract_section_json_line "$artifact_path" "--- ftreplay-header ---")"
footer_json="$(extract_section_json_line "$artifact_path" "--- ftreplay-footer ---")"
if [[ -z "$header_json" || -z "$footer_json" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"validate_sections\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"missing_required_sections\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

expected_timeline_sha="$(jq -r '.integrity.timeline_sha256' <<<"$header_json")"
actual_timeline_sha="$(compute_timeline_sha "$artifact_path")"
if [[ "$expected_timeline_sha" != "$actual_timeline_sha" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"validate_integrity\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"timeline_sha_mismatch\",\"inputs\":{\"expected\":\"$expected_timeline_sha\",\"actual\":\"$actual_timeline_sha\"},\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

if ! jq -e '.schema_version == "ftreplay.v1" and (.content.event_count >= 100) and (.content.decision_count >= 1)' <<<"$header_json" >/dev/null; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"validate_header\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"invalid_header_schema\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

if ! jq -e '.schema_version == "ftreplay.v1" and (.event_count_verified >= 100) and (.integrity_check.timeline_sha256_match == true)' <<<"$footer_json" >/dev/null; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"validate_footer\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"invalid_footer_schema\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"
  exit 1
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"1\",\"step\":\"validate_sections_and_integrity\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"artifact_path\":\"${artifact_path#$ROOT_DIR/}\"}"

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
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"2\",\"step\":\"tamper_detection\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"tamper_not_detected\",\"artifact_path\":\"${tampered_path#$ROOT_DIR/}\"}"
  exit 1
fi
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"2\",\"step\":\"tamper_detection\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"inputs\":{\"expected\":\"$tampered_expected_sha\",\"actual\":\"$tampered_actual_sha\"},\"artifact_path\":\"${tampered_path#$ROOT_DIR/}\"}"

# Scenario 3: recovery path (fresh output rerun restores valid artifact)
scenario3_out="$work_dir/scenario3_recovery/out"
mkdir -p "$scenario3_out"
scenario3_stdout="$raw_dir/scenario3.stdout.json"
scenario3_stderr="$raw_dir/scenario3.stderr.log"
run_harvest_command "$scenario1_src" "$scenario3_out" "incident-only" "$scenario3_stdout" "$scenario3_stderr"

recovered_artifact="$(find "$scenario3_out" -type f -name '*.ftreplay' | head -n 1)"
if [[ -z "$recovered_artifact" || ! -f "$recovered_artifact" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"recovered_artifact_missing\"}"
  exit 1
fi

recovered_header="$(extract_section_json_line "$recovered_artifact" "--- ftreplay-header ---")"
recovered_expected_sha="$(jq -r '.integrity.timeline_sha256' <<<"$recovered_header")"
recovered_actual_sha="$(compute_timeline_sha "$recovered_artifact")"
if [[ "$recovered_expected_sha" != "$recovered_actual_sha" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"recovery_integrity_mismatch\",\"artifact_path\":\"${recovered_artifact#$ROOT_DIR/}\"}"
  exit 1
fi
log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"3\",\"step\":\"recovery\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"artifact_path\":\"${recovered_artifact#$ROOT_DIR/}\"}"

# Scenario 4: chunking path (>100k events)
scenario4_src="$work_dir/scenario4/incidents"
scenario4_out="$work_dir/scenario4/out"
mkdir -p "$scenario4_src" "$scenario4_out"
write_fixture "$scenario4_src/incident_large.jsonl" 100020 yes "large"

scenario4_stdout="$raw_dir/scenario4.stdout.json"
scenario4_stderr="$raw_dir/scenario4.stderr.log"
run_harvest_command "$scenario4_src" "$scenario4_out" "incident-only" "$scenario4_stdout" "$scenario4_stderr"

manifest_path="$(find "$scenario4_out" -type f -name '*.manifest.json' | head -n 1)"
if [[ -z "$manifest_path" || ! -f "$manifest_path" ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"manifest_missing\"}"
  exit 1
fi

chunk_count="$(jq -r '.chunk_count' "$manifest_path")"
if [[ "$chunk_count" -lt 2 ]]; then
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"chunk_count_too_small\",\"inputs\":{\"chunk_count\":$chunk_count},\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"
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
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"fail\",\"run_id\":\"$run_id\",\"error_code\":\"chunk_file_missing\",\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"
  exit 1
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"4\",\"step\":\"chunking\",\"status\":\"pass\",\"run_id\":\"$run_id\",\"inputs\":{\"chunk_count\":$chunk_count},\"artifact_path\":\"${manifest_path#$ROOT_DIR/}\"}"

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_artifact_write_read\",\"scenario_id\":\"$scenario_id\",\"step\":\"complete\",\"status\":\"pass\",\"run_id\":\"$run_id\"}"
echo "Replay artifact write/read e2e passed. Logs: ${json_log#$ROOT_DIR/}"
