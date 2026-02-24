#!/usr/bin/env bash
set -euo pipefail

# Reproduction:
#   bash tests/e2e/test_replay_capture_pipeline.sh
# Expected:
#   - exit 0 when all four replay capture scenarios pass
#   - JSON log at tests/e2e/logs/replay_capture_pipeline_<timestamp>.jsonl

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_capture_pipeline_$(date -u +%Y%m%dT%H%M%SZ)"
json_log="$LOG_DIR/${run_id}.jsonl"
scenarios_pass=0
scenarios_fail=0

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

extract_child_log_path() {
  local raw_log="$1"
  grep -Eo 'Logs: [^ ]+' "$raw_log" | tail -n1 | sed 's/^Logs: //'
}

run_step() {
  local scenario_id="$1"
  local script_name="$2"
  local raw_log="$LOG_DIR/${run_id}.${scenario_id}.raw.log"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"run_child_script\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"${script_name}\",\"inputs\":{\"script\":\"$script_name\"},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${raw_log#$ROOT_DIR/}\"}"

  set +e
  bash "$ROOT_DIR/tests/e2e/$script_name" >"$raw_log" 2>&1
  local rc=$?
  set -e

  local child_log
  child_log="$(extract_child_log_path "$raw_log")"
  if [[ -z "$child_log" ]]; then
    child_log="${raw_log#$ROOT_DIR/}"
  fi

  if [[ $rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"run_child_script\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"${script_name}\",\"inputs\":{\"script\":\"$script_name\",\"error_context\":\"see raw child log\"},\"outcome\":\"failed\",\"reason_code\":\"child_script_failed\",\"error_code\":$rc,\"artifact_path\":\"$child_log\"}"
    tail -n 120 "$raw_log" >&2 || true
    return "$rc"
  fi

  local event_count
  event_count="$(jq -s '[.[] | select(type == "object") | .event_count? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"run_child_script\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"${script_name}\",\"inputs\":{\"script\":\"$script_name\"},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"event_count\":$event_count,\"artifact_path\":\"$child_log\"}"
}

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_start\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

if run_step "1:artifact_structure" "test_replay_capture_extraction.sh"; then
  ((scenarios_pass += 1))
else
  ((scenarios_fail += 1))
fi

if run_step "2:mixed_sensitivity" "test_replay_redaction.sh"; then
  ((scenarios_pass += 1))
else
  ((scenarios_fail += 1))
fi

if run_step "3:compression_and_chunking" "test_replay_artifact_write_read.sh"; then
  ((scenarios_pass += 1))
else
  ((scenarios_fail += 1))
fi

if run_step "4:decision_roundtrip" "test_replay_decision_capture.sh"; then
  ((scenarios_pass += 1))
else
  ((scenarios_fail += 1))
fi

total_events="$(jq -s '[.[] | select(type == "object") | .event_count? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
roundtrip_match=true
suite_status="passed"
suite_outcome="pass"
suite_reason="null"
suite_error="null"
if [[ "$scenarios_fail" -ne 0 ]]; then
  roundtrip_match=false
  suite_status="failed"
  suite_outcome="failed"
  suite_reason="\"one_or_more_scenarios_failed\""
  suite_error="$scenarios_fail"
fi

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_complete\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"$suite_status\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"$suite_outcome\",\"reason_code\":$suite_reason,\"error_code\":$suite_error,\"test\":\"capture_pipeline\",\"scenarios_pass\":$scenarios_pass,\"scenarios_fail\":$scenarios_fail,\"total_events\":$total_events,\"roundtrip_match\":$roundtrip_match,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

if [[ "$scenarios_fail" -ne 0 ]]; then
  echo "Replay capture pipeline e2e failed. Logs: ${json_log#$ROOT_DIR/}" >&2
  exit 1
fi

echo "Replay capture pipeline e2e passed. Logs: ${json_log#$ROOT_DIR/}"
