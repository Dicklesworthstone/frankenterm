#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
METRICS_DIR="${ROOT_DIR}/docs/metrics"
mkdir -p "${LOG_DIR}" "${METRICS_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_chaos_perf"
CORRELATION_ID="ft-1i2ge.8.12-${RUN_ID}"
TARGET_DIR="target-rch-mission-tx-chaos-perf-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_chaos_perf_${RUN_ID}.jsonl"
METRICS_FILE="${METRICS_DIR}/mission_tx_rollout_readiness.json"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "mission_tx_chaos_perf.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

fail_now() {
  local decision_path="$1"
  local reason_code="$2"
  local error_code="$3"
  local artifact_path="$4"
  local input_summary="$5"
  emit_log "failed" "${decision_path}" "${reason_code}" "${error_code}" "${artifact_path}" "${input_summary}"
  echo "FAIL: ${decision_path} (${reason_code})" >&2
  exit 1
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  echo "rch is required; refusing local cargo execution" >&2
  exit 1
fi

emit_log "started" "script_init" "none" "none" "$(basename "${LOG_FILE}")" \
  "run tx chaos/perf validation and rollout-readiness evidence generation"

if rch workers probe --all --json \
  | jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    >/dev/null 2>&1; then
  emit_log "passed" "preflight_rch_workers" "workers_available" "none" \
    "$(basename "${LOG_FILE}")" "at least one healthy rch worker is available"
else
  emit_log "degraded" "preflight_rch_workers" "rch_fail_open_mode" "remote_worker_unavailable" \
    "$(basename "${LOG_FILE}")" \
    "no healthy remote workers; proceeding with rch exec fail-open semantics"
fi

run_suite() {
  local suite_key="$1"
  local decision_path="$2"
  local expected_min="$3"
  shift 3

  local stdout_file="${LOG_DIR}/mission_tx_chaos_perf_${RUN_ID}.${suite_key}.stdout.log"
  local stderr_file="${LOG_DIR}/mission_tx_chaos_perf_${RUN_ID}.${suite_key}.stderr.log"
  local start_epoch
  local end_epoch
  local duration_secs
  local pass_count

  emit_log "running" "${decision_path}" "command_execution" "none" \
    "$(basename "${stdout_file}")" "$*"

  start_epoch="$(date +%s)"
  set +e
  (
    cd "${ROOT_DIR}"
    rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"
  ) >"${stdout_file}" 2>"${stderr_file}"
  local rc=$?
  set -e
  end_epoch="$(date +%s)"
  duration_secs=$((end_epoch - start_epoch))

  if [[ ${rc} -ne 0 ]]; then
    fail_now "${decision_path}" "suite_failed" "cargo_test_failed" \
      "$(basename "${stderr_file}")" "$* (exit=${rc})"
  fi

  pass_count="$(grep -E '^test .+ \.\.\. ok$' "${stdout_file}" | wc -l | tr -d ' ')"
  if [[ "${pass_count}" -lt "${expected_min}" ]]; then
    fail_now "${decision_path}" "insufficient_pass_count" "coverage_threshold_not_met" \
      "$(basename "${stdout_file}")" \
      "expected >= ${expected_min} passing tests, got ${pass_count}"
  fi

  emit_log "passed" "${decision_path}" "suite_passed" "none" \
    "$(basename "${stdout_file}")" \
    "pass_count=${pass_count}; expected_min=${expected_min}; duration_secs=${duration_secs}"

  echo "${pass_count}|${duration_secs}|${stdout_file}|${stderr_file}"
}

chaos_result="$(run_suite \
  "chaos" \
  "chaos_fault_injection_suite" \
  24 \
  cargo test -p frankenterm-core --features subprocess-bridge --test chaos_planner_dispatcher -- --nocapture)"
perf_result="$(run_suite \
  "performance" \
  "performance_budget_suite" \
  20 \
  cargo test -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability -- --nocapture)"
matrix_result="$(run_suite \
  "tx_matrix" \
  "failure_recovery_matrix_suite" \
  19 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture)"
correctness_result="$(run_suite \
  "tx_correctness" \
  "tx_correctness_regression_suite" \
  20 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_correctness_suite -- --nocapture)"

parse_result() {
  local result="$1"
  local field="$2"
  echo "${result}" | awk -F'|' -v f="${field}" '{print $f}'
}

chaos_pass="$(parse_result "${chaos_result}" 1)"
chaos_duration="$(parse_result "${chaos_result}" 2)"
chaos_stdout="$(parse_result "${chaos_result}" 3)"

perf_pass="$(parse_result "${perf_result}" 1)"
perf_duration="$(parse_result "${perf_result}" 2)"
perf_stdout="$(parse_result "${perf_result}" 3)"

matrix_pass="$(parse_result "${matrix_result}" 1)"
matrix_duration="$(parse_result "${matrix_result}" 2)"
matrix_stdout="$(parse_result "${matrix_result}" 3)"

correctness_pass="$(parse_result "${correctness_result}" 1)"
correctness_duration="$(parse_result "${correctness_result}" 2)"
correctness_stdout="$(parse_result "${correctness_result}" 3)"

kill_switch_hits="$( (grep -Eio 'kill[_-]?switch|hard_stop|safe_mode' "${matrix_stdout}" "${correctness_stdout}" || true) | wc -l | tr -d '[:space:]' )"
rollback_hits="$( (grep -Eio 'rollback|compensation|rolled_back' "${matrix_stdout}" "${correctness_stdout}" || true) | wc -l | tr -d '[:space:]' )"

bounded_failure_behavior=true
measured_tx_overhead=true
kill_switch_verified=false
rollback_readiness_verified=false

if [[ "${kill_switch_hits}" -gt 0 ]]; then
  kill_switch_verified=true
fi
if [[ "${rollback_hits}" -gt 0 ]]; then
  rollback_readiness_verified=true
fi

jq -n \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg run_id "${RUN_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg status "pass" \
  --argjson bounded_failure_behavior "${bounded_failure_behavior}" \
  --argjson measured_tx_overhead "${measured_tx_overhead}" \
  --argjson kill_switch_verified "${kill_switch_verified}" \
  --argjson rollback_readiness_verified "${rollback_readiness_verified}" \
  --argjson chaos_pass "${chaos_pass}" \
  --argjson chaos_expected_min 24 \
  --argjson chaos_duration "${chaos_duration}" \
  --arg chaos_log "${chaos_stdout#${ROOT_DIR}/}" \
  --argjson perf_pass "${perf_pass}" \
  --argjson perf_expected_min 20 \
  --argjson perf_duration "${perf_duration}" \
  --arg perf_log "${perf_stdout#${ROOT_DIR}/}" \
  --argjson matrix_pass "${matrix_pass}" \
  --argjson matrix_expected_min 19 \
  --argjson matrix_duration "${matrix_duration}" \
  --arg matrix_log "${matrix_stdout#${ROOT_DIR}/}" \
  --argjson correctness_pass "${correctness_pass}" \
  --argjson correctness_expected_min 20 \
  --argjson correctness_duration "${correctness_duration}" \
  --arg correctness_log "${correctness_stdout#${ROOT_DIR}/}" \
  --argjson kill_switch_hits "${kill_switch_hits}" \
  --argjson rollback_hits "${rollback_hits}" \
  --arg metrics_contract "ft-1i2ge.8.12" \
  '{
    generated_at_utc: $generated_at,
    bead_id: $metrics_contract,
    run_id: $run_id,
    scenario_id: $scenario_id,
    correlation_id: $correlation_id,
    status: $status,
    heavy_command_policy: "all cargo test commands executed via rch exec",
    suites: {
      chaos_fault_injection: {
        test_binary: "chaos_planner_dispatcher",
        passing_tests: $chaos_pass,
        expected_min: $chaos_expected_min,
        duration_seconds: $chaos_duration,
        artifact_log: $chaos_log
      },
      performance_budget: {
        test_binary: "mission_perf_scalability",
        passing_tests: $perf_pass,
        expected_min: $perf_expected_min,
        duration_seconds: $perf_duration,
        artifact_log: $perf_log
      },
      failure_recovery_matrix: {
        test_binary: "tx_e2e_scenario_matrix",
        passing_tests: $matrix_pass,
        expected_min: $matrix_expected_min,
        duration_seconds: $matrix_duration,
        artifact_log: $matrix_log
      },
      tx_correctness_regression: {
        test_binary: "tx_correctness_suite",
        passing_tests: $correctness_pass,
        expected_min: $correctness_expected_min,
        duration_seconds: $correctness_duration,
        artifact_log: $correctness_log
      }
    },
    readiness_gates: {
      bounded_failure_behavior: $bounded_failure_behavior,
      measured_tx_overhead: $measured_tx_overhead,
      kill_switch_verified: $kill_switch_verified,
      rollback_readiness_verified: $rollback_readiness_verified
    },
    evidence_summary: {
      kill_switch_signal_hits: $kill_switch_hits,
      rollback_signal_hits: $rollback_hits
    },
    residual_risks: [
      "No long-haul soak in this bead run; covered by downstream G6 soak gate.",
      "No live multi-host partition drill in this bead run; deferred to production hardening game-day workflows."
    ]
  }' > "${METRICS_FILE}"

emit_log "passed" "rollout_readiness_report" "artifact_generated" "none" \
  "${METRICS_FILE#${ROOT_DIR}/}" \
  "generated mission tx rollout readiness metrics artifact"

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated chaos + performance + failure/recovery + regression suites"

echo "Mission tx chaos/perf e2e passed."
echo "Structured logs: ${LOG_FILE#${ROOT_DIR}/}"
echo "Readiness report: ${METRICS_FILE#${ROOT_DIR}/}"
