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
component="replay_capture_pipeline"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log_json() {
  echo "$1" >>"$json_log"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_preflight\",\"pane_id\":null,\"step\":\"prereq_check\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"command\":\"$cmd\"},\"outcome\":\"failed\",\"reason_code\":\"missing_prerequisite\",\"error_code\":\"E2E-PREREQ\",\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"
    echo "missing required command: $cmd" >&2
    exit 1
  fi
}

probe_rch_workers() {
  local probe_log="$LOG_DIR/${run_id}.rch_probe.json"

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_preflight\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"running\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"running\",\"reason_code\":null,\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"

  set +e
  rch workers probe --all --json >"$probe_log" 2>&1
  local probe_rc=$?
  set -e

  if [[ $probe_rc -ne 0 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_preflight\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{},\"outcome\":\"failed\",\"reason_code\":\"rch_probe_failed\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "rch workers probe failed" >&2
    exit 2
  fi

  local healthy_workers
  healthy_workers="$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "$probe_log" 2>/dev/null || echo 0)"
  if [[ "$healthy_workers" -lt 1 ]]; then
    log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_preflight\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"failed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"failed\",\"reason_code\":\"rch_workers_unreachable\",\"error_code\":\"RCH-E100\",\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  fi

  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"$component\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_preflight\",\"pane_id\":null,\"step\":\"rch_probe\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"preflight\",\"inputs\":{\"healthy_workers\":$healthy_workers},\"outcome\":\"pass\",\"reason_code\":\"workers_reachable\",\"error_code\":null,\"artifact_path\":\"${probe_log#$ROOT_DIR/}\"}"
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
  local decisions_captured
  local secrets_found
  local secrets_redacted
  local read_events
  local compression_ratio
  event_count="$(jq -s '[.[] | select(type == "object") | .event_count? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  decisions_captured="$(jq -s '[.[] | select(type == "object") | .decisions_captured? // .decision_count? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  secrets_found="$(jq -s '[.[] | select(type == "object") | .secrets_found? // .secrets_detected? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  secrets_redacted="$(jq -s '[.[] | select(type == "object") | .secrets_redacted? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  read_events="$(jq -s '[.[] | select(type == "object") | .read_events? // empty] | add // 0' "$child_log" 2>/dev/null || echo 0)"
  compression_ratio="$(jq -s '[.[] | select(type == "object") | .compression_ratio? // empty] | max // null' "$child_log" 2>/dev/null || echo null)"
  log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"$scenario_id\",\"pane_id\":null,\"step\":\"run_child_script\",\"status\":\"passed\",\"correlation_id\":\"$run_id\",\"decision_path\":\"${script_name}\",\"inputs\":{\"script\":\"$script_name\"},\"outcome\":\"pass\",\"reason_code\":null,\"error_code\":null,\"event_count\":$event_count,\"capture_events\":$event_count,\"decisions_captured\":$decisions_captured,\"secrets_detected\":$secrets_found,\"secrets_redacted\":$secrets_redacted,\"read_events\":$read_events,\"compression_ratio\":$compression_ratio,\"artifact_path\":\"$child_log\"}"
}

require_cmd jq
require_cmd rch
require_cmd cargo
probe_rch_workers

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
capture_events="$(jq -s '[.[] | select(type == "object") | .capture_events? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
decisions_captured="$(jq -s '[.[] | select(type == "object") | .decisions_captured? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
secrets_detected="$(jq -s '[.[] | select(type == "object") | .secrets_detected? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
secrets_redacted="$(jq -s '[.[] | select(type == "object") | .secrets_redacted? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
read_events="$(jq -s '[.[] | select(type == "object") | .read_events? // empty] | add // 0' "$json_log" 2>/dev/null || echo 0)"
compression_ratio="$(jq -s '[.[] | select(type == "object") | .compression_ratio? // empty] | max // null' "$json_log" 2>/dev/null || echo null)"
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

log_json "{\"timestamp\":\"$(now_ts)\",\"component\":\"replay_capture_pipeline\",\"run_id\":\"$run_id\",\"scenario_id\":\"suite_complete\",\"pane_id\":null,\"step\":\"suite\",\"status\":\"$suite_status\",\"correlation_id\":\"$run_id\",\"decision_path\":\"suite\",\"inputs\":{},\"outcome\":\"$suite_outcome\",\"reason_code\":$suite_reason,\"error_code\":$suite_error,\"test\":\"capture_pipeline\",\"scenarios_pass\":$scenarios_pass,\"scenarios_fail\":$scenarios_fail,\"total_events\":$total_events,\"capture_events\":$capture_events,\"decisions_captured\":$decisions_captured,\"secrets_detected\":$secrets_detected,\"secrets_redacted\":$secrets_redacted,\"compression_ratio\":$compression_ratio,\"read_events\":$read_events,\"roundtrip_match\":$roundtrip_match,\"artifact_path\":\"${json_log#$ROOT_DIR/}\"}"

if [[ "$scenarios_fail" -ne 0 ]]; then
  echo "Replay capture pipeline e2e failed. Logs: ${json_log#$ROOT_DIR/}" >&2
  exit 1
fi

echo "Replay capture pipeline e2e passed. Logs: ${json_log#$ROOT_DIR/}"
