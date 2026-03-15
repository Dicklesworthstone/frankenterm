#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/tests/e2e/logs"
mkdir -p "$LOG_DIR"

run_id="replay_performance_$(date -u +%Y%m%dT%H%M%SZ)"
scenario_id="replay_performance"
json_log="$LOG_DIR/${run_id}.jsonl"
raw_dir="$LOG_DIR/${run_id}_raw"
mkdir -p "$raw_dir"

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_target_dir="${CARGO_TARGET_DIR:-$ROOT_DIR/target-replay-performance}-${run_id}"
mkdir -p "$cargo_home" "$cargo_target_dir"

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/replay_performance_${run_id}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/replay_performance_${run_id}.smoke.log"
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

pass_count=0
fail_count=0
suite_status=0
suite_started_ms="$(date +%s)"

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
        cd "$ROOT_DIR"
        env TMPDIR=/tmp "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${RCH_STEP_TIMEOUT_SECS}" \
            rch exec -- env \
            CARGO_HOME="$cargo_home" \
            CARGO_TARGET_DIR="$cargo_target_dir" \
            cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e

    check_rch_fallback "${output_file}"
    if [[ ${rc} -eq 124 || ${rc} -eq 137 ]]; then
        local queue_log="${output_file%.log}.rch_queue_timeout.log"
        if ! run_rch queue >"${queue_log}" 2>&1; then
            queue_log="${output_file}"
        fi
        fatal "RCH-REMOTE-STALL: rch remote command timed out after ${RCH_STEP_TIMEOUT_SECS}s; refusing stalled remote execution. See ${queue_log}"
    fi
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this replay e2e harness; refusing local cargo execution."
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
    run_rch_cargo_logged "${RCH_SMOKE_LOG}" check --help
    local smoke_rc=$?
    set -e
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_ms() {
  echo $(( $(date +%s) * 1000 ))
}

log_json() {
  local payload="$1"
  echo "$payload" >> "$json_log"
}

emit_metric_log() {
  local scenario="$1"
  local metric="$2"
  local value="$3"
  local budget="$4"
  local within_budget="$5"
  local status="$6"
  local reason_code="$7"
  local artifact_path="$8"

  local payload
  payload="$(jq -nc \
    --arg timestamp "$(now_ts)" \
    --arg component "replay_performance" \
    --arg scenario_id "$scenario_id" \
    --arg correlation_id "$run_id" \
    --argjson scenario "$scenario" \
    --arg metric "$metric" \
    --argjson value "$value" \
    --argjson budget "$budget" \
    --argjson within_budget "$within_budget" \
    --arg status "$status" \
    --arg reason_code "$reason_code" \
    --arg artifact_path "$artifact_path" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      test: "performance",
      scenario: $scenario,
      metric: $metric,
      value: $value,
      budget: $budget,
      within_budget: $within_budget,
      status: $status,
      reason_code: $reason_code,
      artifact_path: $artifact_path
    }')"
  log_json "$payload"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_json "$(jq -nc --arg timestamp "$(now_ts)" --arg cmd "$cmd" '{timestamp:$timestamp,test:"performance",status:"fail",reason_code:"missing_command",metric:$cmd}')"
    exit 1
  fi
}

extract_median_ns() {
  local estimates_file="$1"
  jq -er '.median.point_estimate' "$estimates_file"
}

to_ms() {
  local ns="$1"
  awk "BEGIN { printf \"%.6f\", ($ns / 1000000.0) }"
}

to_eps() {
  local events="$1"
  local ns="$2"
  awk "BEGIN { if ($ns <= 0) print 0; else printf \"%.6f\", (($events * 1000000000.0) / $ns) }"
}

run_bench() {
  local scenario="$1"
  local bench_name="$2"
  local combined_file="$raw_dir/scenario${scenario}.${bench_name}.combined.log"

  set +e
  run_rch_cargo_logged "${combined_file}" bench -p frankenterm-core --bench "$bench_name"
  local rc=$?
  set -e

  if [[ $rc -ne 0 ]]; then
    emit_metric_log "$scenario" "${bench_name}_execution" "0" "1" "false" "fail" "cargo_bench_failed" "${combined_file#$ROOT_DIR/}"
    tail -n 120 "$combined_file" >&2 || true
    suite_status=1
    fail_count=$((fail_count + 1))
    return 1
  fi

  return 0
}

require_cmd jq
ensure_rch_ready

log_json "$(jq -nc --arg timestamp "$(now_ts)" --arg run_id "$run_id" --arg scenario_id "$scenario_id" '{timestamp:$timestamp,test:"performance",scenario_id:$scenario_id,correlation_id:$run_id,status:"running",step:"start"}')"

# Scenario 1: capture benchmark budget (<1ms/event)
if run_bench 1 "replay_capture"; then
  capture_estimate="$cargo_target_dir/criterion/replay_capture/capture_overhead_per_event/new/estimates.json"
  if [[ -f "$capture_estimate" ]]; then
    capture_ns="$(extract_median_ns "$capture_estimate")"
    capture_ms="$(to_ms "$capture_ns")"
    within_budget="$(awk "BEGIN { print ($capture_ms <= 1.0) ? \"true\" : \"false\" }")"
    status="pass"
    reason_code="within_budget"
    if [[ "$within_budget" != "true" ]]; then
      status="fail"
      reason_code="capture_over_budget"
      suite_status=1
      fail_count=$((fail_count + 1))
    else
      pass_count=$((pass_count + 1))
    fi
    emit_metric_log "1" "capture_overhead_ms" "$capture_ms" "1.0" "$within_budget" "$status" "$reason_code" "${capture_estimate#$ROOT_DIR/}"
  else
    emit_metric_log "1" "capture_overhead_ms" "0" "1.0" "false" "fail" "missing_estimate" "${capture_estimate#$ROOT_DIR/}"
    suite_status=1
    fail_count=$((fail_count + 1))
  fi
fi

# Scenario 2: replay throughput benchmark (>=100K events/sec)
if run_bench 2 "replay_kernel"; then
  kernel_estimate="$cargo_target_dir/criterion/replay_kernel/instant_mode_20000_events/new/estimates.json"
  if [[ -f "$kernel_estimate" ]]; then
    kernel_ns="$(extract_median_ns "$kernel_estimate")"
    replay_eps="$(to_eps 20000 "$kernel_ns")"
    within_budget="$(awk "BEGIN { print ($replay_eps >= 100000.0) ? \"true\" : \"false\" }")"
    status="pass"
    reason_code="within_budget"
    if [[ "$within_budget" != "true" ]]; then
      status="fail"
      reason_code="replay_throughput_below_budget"
      suite_status=1
      fail_count=$((fail_count + 1))
    else
      pass_count=$((pass_count + 1))
    fi
    emit_metric_log "2" "replay_throughput_eps" "$replay_eps" "100000.0" "$within_budget" "$status" "$reason_code" "${kernel_estimate#$ROOT_DIR/}"
  else
    emit_metric_log "2" "replay_throughput_eps" "0" "100000.0" "false" "fail" "missing_estimate" "${kernel_estimate#$ROOT_DIR/}"
    suite_status=1
    fail_count=$((fail_count + 1))
  fi
fi

# Scenario 3: diff benchmark budget (<1s for 1000 divergences)
if run_bench 3 "replay_diff"; then
  diff_estimate="$cargo_target_dir/criterion/replay_diff/diff_1000_divergences/new/estimates.json"
  if [[ -f "$diff_estimate" ]]; then
    diff_ns="$(extract_median_ns "$diff_estimate")"
    diff_ms="$(to_ms "$diff_ns")"
    within_budget="$(awk "BEGIN { print ($diff_ms <= 1000.0) ? \"true\" : \"false\" }")"
    status="pass"
    reason_code="within_budget"
    if [[ "$within_budget" != "true" ]]; then
      status="fail"
      reason_code="diff_latency_over_budget"
      suite_status=1
      fail_count=$((fail_count + 1))
    else
      pass_count=$((pass_count + 1))
    fi
    emit_metric_log "3" "diff_latency_ms" "$diff_ms" "1000.0" "$within_budget" "$status" "$reason_code" "${diff_estimate#$ROOT_DIR/}"
  else
    emit_metric_log "3" "diff_latency_ms" "0" "1000.0" "false" "fail" "missing_estimate" "${diff_estimate#$ROOT_DIR/}"
    suite_status=1
    fail_count=$((fail_count + 1))
  fi
fi

# Scenario 4: baseline regression evaluation (warning >10%, blocking >25%)
scenario4_dir="$raw_dir/scenario4_gate"
mkdir -p "$scenario4_dir"
scenario4_stdout="$scenario4_dir/stdout.log"
scenario4_stderr="$scenario4_dir/stderr.log"
set +e
"$ROOT_DIR/scripts/check_replay_performance_gates.sh" \
  --check \
  --criterion-dir "$cargo_target_dir/criterion" \
  --artifacts-dir "$scenario4_dir" \
  --baseline-file "$ROOT_DIR/evidence/ft-og6q6.7.3/replay_performance_baseline.json" \
  >"$scenario4_stdout" 2>"$scenario4_stderr"
scenario4_rc=$?
set -e

if [[ $scenario4_rc -ne 0 ]]; then
  emit_metric_log "4" "baseline_regression_gate" "1" "0" "false" "fail" "blocking_regression" "${scenario4_stderr#$ROOT_DIR/}"
  suite_status=1
  fail_count=$((fail_count + 1))
else
  report_file="$scenario4_dir/replay-performance-report.json"
  if [[ -f "$report_file" ]]; then
    overall_status="$(jq -r '.summary.overall_status' "$report_file")"
    warning_count="$(jq -r '.summary.warning_count' "$report_file")"
    within_budget="true"
    status="pass"
    reason_code="baseline_compare_pass"
    if [[ "$overall_status" == "warning" ]]; then
      reason_code="baseline_compare_warning"
    fi
    emit_metric_log "4" "baseline_regression_gate" "$warning_count" "0" "$within_budget" "$status" "$reason_code" "${report_file#$ROOT_DIR/}"
    pass_count=$((pass_count + 1))
  else
    emit_metric_log "4" "baseline_regression_gate" "0" "0" "false" "fail" "missing_gate_report" "${scenario4_dir#$ROOT_DIR/}"
    suite_status=1
    fail_count=$((fail_count + 1))
  fi
fi

suite_duration_ms=$(( ($(date +%s) - suite_started_ms) * 1000 ))
summary_json="$(jq -nc \
  --arg test "performance" \
  --arg status "$([[ $suite_status -eq 0 ]] && echo pass || echo fail)" \
  --argjson pass "$pass_count" \
  --argjson fail "$fail_count" \
  --argjson duration_ms "$suite_duration_ms" \
  '{test:$test,pass:$pass,fail:$fail,status:$status,duration_ms:$duration_ms}'
)"

log_json "$(jq -nc --arg timestamp "$(now_ts)" --argjson summary "$summary_json" '{timestamp:$timestamp,test:"performance",step:"complete",summary:$summary}')"

echo "$summary_json"

if [[ $suite_status -ne 0 ]]; then
  exit 1
fi
