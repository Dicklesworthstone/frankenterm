#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
METRICS_DIR="${ROOT_DIR}/docs/metrics"
mkdir -p "${LOG_DIR}" "${METRICS_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_soak"
CORRELATION_ID="ft-1i2ge.7.6-soak-${RUN_ID}"
TX_ID="tx-soak-${RUN_ID}"
FIXED_SEED="1706001"
TARGET_DIR="target-rch-mission-e2e"
LOG_FILE="${LOG_DIR}/mission_soak_${RUN_ID}.jsonl"
PROBE_FILE="${LOG_DIR}/mission_soak_${RUN_ID}.rch_probe.json"
REPORT_FILE="${METRICS_DIR}/mission_soak_evidence.json"

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
    --arg component "mission_soak.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg tx_id "${TX_ID}" \
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
      tx_id: $tx_id,
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

run_suite() {
  local suite_key="$1"
  local decision_path="$2"
  local expected_min="$3"
  shift 3

  local stdout_file="${LOG_DIR}/mission_soak_${RUN_ID}.${suite_key}.stdout.log"
  local stderr_file="${LOG_DIR}/mission_soak_${RUN_ID}.${suite_key}.stderr.log"
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
    rch exec -- env \
      CARGO_TARGET_DIR="${TARGET_DIR}" \
      FT_MISSION_TEST_SEED="${FIXED_SEED}" \
      PROPTEST_CASES=40 \
      RUST_TEST_THREADS=1 \
      "$@"
  ) >"${stdout_file}" 2>"${stderr_file}"
  local rc=$?
  set -e
  end_epoch="$(date +%s)"
  duration_secs=$((end_epoch - start_epoch))

  if [[ ${rc} -ne 0 ]]; then
    fail_now "${decision_path}" "suite_failed" "cargo_test_failed" \
      "$(basename "${stderr_file}")" "$* (exit=${rc})"
  fi

  pass_count="$(cat "${stdout_file}" "${stderr_file}" | grep -E '^test .+ \.\.\. ok$' | wc -l | tr -d ' ')"
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

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  echo "rch is required; refusing local cargo execution" >&2
  exit 1
fi

emit_log "started" "script_init" "none" "none" "$(basename "${LOG_FILE}")" \
  "long-haul soak campaign with deterministic seed and tx-linked structured logs"

if rch workers probe --all --json >"${PROBE_FILE}" 2>&1 \
  && jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    "${PROBE_FILE}" >/dev/null 2>&1; then
  emit_log "passed" "preflight_rch_workers" "workers_available" "none" \
    "$(basename "${PROBE_FILE}")" "healthy rch workers available"
else
  emit_log "degraded" "preflight_rch_workers" "rch_fail_open_mode" "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" "no healthy remote workers; rch fail-open may execute locally"
fi

baseline_perf_result="$(run_suite \
  "baseline_perf" \
  "soak_baseline_perf_budget" \
  20 \
  cargo test -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability -- --nocapture)"
baseline_matrix_result="$(run_suite \
  "baseline_tx_matrix" \
  "soak_baseline_tx_matrix" \
  19 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture)"

parse_result() {
  local result="$1"
  local field="$2"
  echo "${result}" | awk -F'|' -v f="${field}" '{print $f}'
}

baseline_perf_pass="$(parse_result "${baseline_perf_result}" 1)"
baseline_perf_duration="$(parse_result "${baseline_perf_result}" 2)"
baseline_matrix_pass="$(parse_result "${baseline_matrix_result}" 1)"
baseline_matrix_duration="$(parse_result "${baseline_matrix_result}" 2)"

baseline_resume_pass=0
baseline_cycle_duration=0
deterministic_resume=true
bounded_degradation=true

declare -a cycle_durations=()
declare -a cycle_resume_passes=()

for cycle in 1 2 3; do
  resume_result="$(run_suite \
    "cycle_${cycle}_resume" \
    "soak_cycle_resume_after_restart" \
    4 \
    cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture resume_)"
  rollback_result="$(run_suite \
    "cycle_${cycle}_rollback" \
    "soak_cycle_rollback_storm" \
    1 \
    cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture sc6_auto_rollback_success)"
  perf_guard_result="$(run_suite \
    "cycle_${cycle}_perf_guard" \
    "soak_cycle_perf_guard" \
    1 \
    cargo test -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability -- --nocapture determinism_cycle_count_identical)"

  resume_pass="$(parse_result "${resume_result}" 1)"
  resume_duration="$(parse_result "${resume_result}" 2)"
  rollback_duration="$(parse_result "${rollback_result}" 2)"
  perf_guard_duration="$(parse_result "${perf_guard_result}" 2)"
  cycle_duration=$((resume_duration + rollback_duration + perf_guard_duration))

  cycle_durations+=("${cycle_duration}")
  cycle_resume_passes+=("${resume_pass}")

  if [[ ${cycle} -eq 1 ]]; then
    baseline_resume_pass="${resume_pass}"
    baseline_cycle_duration="${cycle_duration}"
    if [[ "${baseline_cycle_duration}" -lt 1 ]]; then
      baseline_cycle_duration=1
    fi
  else
    if [[ "${resume_pass}" -ne "${baseline_resume_pass}" ]]; then
      deterministic_resume=false
    fi
    if [[ "${cycle_duration}" -gt $((baseline_cycle_duration * 4)) ]]; then
      bounded_degradation=false
    fi
  fi
done

if [[ "${deterministic_resume}" != "true" ]]; then
  fail_now "soak_resume_determinism_assertion" "resume_nondeterministic" "resume_variation_detected" \
    "$(basename "${LOG_FILE}")" \
    "resume pass-count changed across fixed-seed cycles"
fi

if [[ "${bounded_degradation}" != "true" ]]; then
  fail_now "soak_degradation_assertion" "unbounded_degradation_detected" "duration_budget_exceeded" \
    "$(basename "${LOG_FILE}")" \
    "cycle duration exceeded 4x baseline"
fi

cycle_durations_json="$(printf '%s\n' "${cycle_durations[@]}" | jq -Rsc 'split("\n")[:-1] | map(tonumber)')"
cycle_resume_json="$(printf '%s\n' "${cycle_resume_passes[@]}" | jq -Rsc 'split("\n")[:-1] | map(tonumber)')"

jq -n \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg bead_id "ft-1i2ge.7.6" \
  --arg run_id "${RUN_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg tx_id "${TX_ID}" \
  --arg fixed_seed "${FIXED_SEED}" \
  --arg heavy_command_policy "all cargo test commands executed via rch exec" \
  --argjson baseline_perf_pass "${baseline_perf_pass}" \
  --argjson baseline_perf_duration "${baseline_perf_duration}" \
  --argjson baseline_matrix_pass "${baseline_matrix_pass}" \
  --argjson baseline_matrix_duration "${baseline_matrix_duration}" \
  --argjson deterministic_resume "${deterministic_resume}" \
  --argjson bounded_degradation "${bounded_degradation}" \
  --argjson cycle_durations "${cycle_durations_json}" \
  --argjson cycle_resume "${cycle_resume_json}" \
  '{
    generated_at_utc: $generated_at,
    bead_id: $bead_id,
    run_id: $run_id,
    scenario_id: $scenario_id,
    correlation_id: $correlation_id,
    tx_id: $tx_id,
    fixed_seed: $fixed_seed,
    heavy_command_policy: $heavy_command_policy,
    baseline: {
      perf_suite_pass_count: $baseline_perf_pass,
      perf_suite_duration_seconds: $baseline_perf_duration,
      tx_matrix_pass_count: $baseline_matrix_pass,
      tx_matrix_duration_seconds: $baseline_matrix_duration
    },
    soak_cycles: {
      count: 3,
      cycle_total_duration_seconds: $cycle_durations,
      resume_pass_counts: $cycle_resume
    },
    assertions: {
      deterministic_resume_behavior: $deterministic_resume,
      no_unbounded_degradation: $bounded_degradation
    },
    residual_risks: [
      "Campaign uses deterministic fixed-seed simulation windows rather than multi-hour wall-clock runtime.",
      "Cross-host partition failures are validated in dedicated infrastructure drills, not this soak harness."
    ]
  }' > "${REPORT_FILE}"

emit_log "passed" "soak_evidence_bundle" "artifact_generated" "none" \
  "${REPORT_FILE#${ROOT_DIR}/}" "generated deterministic soak evidence bundle"

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "long-haul soak campaign completed with bounded degradation and deterministic resume assertions"

echo "Mission soak e2e passed."
echo "Structured logs: ${LOG_FILE#${ROOT_DIR}/}"
echo "Soak report: ${REPORT_FILE#${ROOT_DIR}/}"
