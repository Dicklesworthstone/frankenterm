#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
METRICS_DIR="${ROOT_DIR}/docs/metrics"
mkdir -p "${LOG_DIR}" "${METRICS_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_chaos"
CORRELATION_ID="ft-1i2ge.7.6-chaos-${RUN_ID}"
TX_ID="tx-chaos-${RUN_ID}"
FIXED_SEED="1706001"
TARGET_DIR="target-rch-mission-e2e"
LOG_FILE="${LOG_DIR}/mission_chaos_${RUN_ID}.jsonl"
PROBE_FILE="${LOG_DIR}/mission_chaos_${RUN_ID}.rch_probe.json"
REPORT_FILE="${METRICS_DIR}/mission_chaos_evidence.json"

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
    --arg component "mission_chaos.e2e" \
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

  local stdout_file="${LOG_DIR}/mission_chaos_${RUN_ID}.${suite_key}.stdout.log"
  local stderr_file="${LOG_DIR}/mission_chaos_${RUN_ID}.${suite_key}.stderr.log"
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

extract_passed_tests() {
  local stdout_file="$1"
  local stderr_file="$2"
  cat "${stdout_file}" "${stderr_file}" \
    | grep -E '^test .+ \.\.\. ok$' \
    | sed -E 's/^test ([^ ]+) \.\.\. ok$/\1/' \
    | LC_ALL=C sort
}

parse_result() {
  local result="$1"
  local field="$2"
  echo "${result}" | awk -F'|' -v f="${field}" '{print $f}'
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
  "chaos burn-in campaign with fixed-seed rollback storm + recovery checks"

if rch workers probe --all --json >"${PROBE_FILE}" 2>&1 \
  && jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    "${PROBE_FILE}" >/dev/null 2>&1; then
  emit_log "passed" "preflight_rch_workers" "workers_available" "none" \
    "$(basename "${PROBE_FILE}")" "healthy rch workers available"
else
  emit_log "degraded" "preflight_rch_workers" "rch_fail_open_mode" "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" "no healthy remote workers; rch fail-open may execute locally"
fi

chaos_result="$(run_suite \
  "chaos_planner_dispatcher" \
  "chaos_fault_injection_matrix" \
  20 \
  cargo test -p frankenterm-core --features subprocess-bridge --test chaos_planner_dispatcher -- --nocapture)"
tx_matrix_result="$(run_suite \
  "tx_scenario_matrix" \
  "chaos_tx_scenario_matrix" \
  19 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture)"
observability_result="$(run_suite \
  "tx_observability" \
  "chaos_evidence_bundling_logic" \
  10 \
  cargo test -p frankenterm-core --features subprocess-bridge --test proptest_tx_observability -- --nocapture)"

resume_a_result="$(run_suite \
  "resume_recovery_a" \
  "chaos_resume_recovery_run_a" \
  4 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture resume_)"
resume_b_result="$(run_suite \
  "resume_recovery_b" \
  "chaos_resume_recovery_run_b" \
  4 \
  cargo test -p frankenterm-core --features subprocess-bridge --test tx_e2e_scenario_matrix -- --nocapture resume_)"

chaos_stdout="$(parse_result "${chaos_result}" 3)"
tx_matrix_stdout="$(parse_result "${tx_matrix_result}" 3)"
observability_stdout="$(parse_result "${observability_result}" 3)"
resume_a_stdout="$(parse_result "${resume_a_result}" 3)"
resume_b_stdout="$(parse_result "${resume_b_result}" 3)"
chaos_stderr="$(parse_result "${chaos_result}" 4)"
tx_matrix_stderr="$(parse_result "${tx_matrix_result}" 4)"
observability_stderr="$(parse_result "${observability_result}" 4)"
resume_a_stderr="$(parse_result "${resume_a_result}" 4)"
resume_b_stderr="$(parse_result "${resume_b_result}" 4)"

resume_a_pass="$(parse_result "${resume_a_result}" 1)"
resume_b_pass="$(parse_result "${resume_b_result}" 1)"
resume_a_duration="$(parse_result "${resume_a_result}" 2)"
resume_b_duration="$(parse_result "${resume_b_result}" 2)"

if [[ "${resume_a_pass}" -ne "${resume_b_pass}" ]]; then
  fail_now "chaos_resume_determinism_assertion" "resume_pass_count_mismatch" "resume_nondeterministic" \
    "$(basename "${resume_a_stdout}")" \
    "resume pass count differs between fixed-seed runs: ${resume_a_pass} vs ${resume_b_pass}"
fi

resume_a_tests="$(extract_passed_tests "${resume_a_stdout}" "${resume_a_stderr}")"
resume_b_tests="$(extract_passed_tests "${resume_b_stdout}" "${resume_b_stderr}")"
if [[ "${resume_a_tests}" != "${resume_b_tests}" ]]; then
  fail_now "chaos_resume_determinism_assertion" "resume_test_set_mismatch" "resume_nondeterministic" \
    "$(basename "${resume_b_stdout}")" \
    "resume passed-test set differs between fixed-seed runs"
fi

if (( resume_b_duration > (resume_a_duration < 1 ? 1 : resume_a_duration) * 4 )); then
  fail_now "chaos_degradation_assertion" "unbounded_degradation_detected" "duration_budget_exceeded" \
    "$(basename "${resume_b_stdout}")" \
    "resume run B duration exceeded 4x run A duration"
fi

required_markers=(
  "test sc5_mid_commit_failure ... ok"
  "test sc6_auto_rollback_success ... ok"
  "test sc7_partial_rollback_failure ... ok"
  "test sc8_forced_rollback_kill_switch ... ok"
  "test resume_after_partial_commit ... ok"
  "test resume_after_full_pipeline ... ok"
)
for marker in "${required_markers[@]}"; do
  if ! grep -Fq "${marker}" "${tx_matrix_stdout}" "${tx_matrix_stderr}"; then
    fail_now "chaos_recovery_coverage_assertion" "missing_recovery_marker" "rollback_or_resume_gap" \
      "$(basename "${tx_matrix_stdout}")" \
      "missing expected tx recovery marker: ${marker}"
  fi
done

if grep -Eiq "unsafe[_ -]?dispatch[_ -]?escalation|escalation=unsafe" \
  "${chaos_stdout}" "${chaos_stderr}" "${tx_matrix_stdout}" "${tx_matrix_stderr}" \
  "${resume_a_stdout}" "${resume_a_stderr}" "${resume_b_stdout}" "${resume_b_stderr}" 2>/dev/null; then
  fail_now "chaos_safety_assertion" "unsafe_dispatch_escalation_detected" "safety_violation" \
    "$(basename "${chaos_stdout}")" \
    "unsafe dispatch escalation signature detected"
fi

jq -n \
  --arg generated_at "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
  --arg bead_id "ft-1i2ge.7.6" \
  --arg run_id "${RUN_ID}" \
  --arg scenario_id "${SCENARIO_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg tx_id "${TX_ID}" \
  --arg fixed_seed "${FIXED_SEED}" \
  --arg heavy_command_policy "all cargo test commands executed via rch exec" \
  --arg chaos_log "${chaos_stdout#${ROOT_DIR}/}" \
  --arg tx_matrix_log "${tx_matrix_stdout#${ROOT_DIR}/}" \
  --arg observability_log "${observability_stdout#${ROOT_DIR}/}" \
  --arg resume_a_log "${resume_a_stdout#${ROOT_DIR}/}" \
  --arg resume_b_log "${resume_b_stdout#${ROOT_DIR}/}" \
  --argjson chaos_pass "$(parse_result "${chaos_result}" 1)" \
  --argjson tx_matrix_pass "$(parse_result "${tx_matrix_result}" 1)" \
  --argjson observability_pass "$(parse_result "${observability_result}" 1)" \
  --argjson resume_a_pass "${resume_a_pass}" \
  --argjson resume_b_pass "${resume_b_pass}" \
  --argjson resume_a_duration "${resume_a_duration}" \
  --argjson resume_b_duration "${resume_b_duration}" \
  '{
    generated_at_utc: $generated_at,
    bead_id: $bead_id,
    run_id: $run_id,
    scenario_id: $scenario_id,
    correlation_id: $correlation_id,
    tx_id: $tx_id,
    fixed_seed: $fixed_seed,
    heavy_command_policy: $heavy_command_policy,
    suites: {
      chaos_planner_dispatcher_pass_count: $chaos_pass,
      tx_scenario_matrix_pass_count: $tx_matrix_pass,
      tx_observability_pass_count: $observability_pass,
      resume_run_a_pass_count: $resume_a_pass,
      resume_run_b_pass_count: $resume_b_pass,
      resume_run_a_duration_seconds: $resume_a_duration,
      resume_run_b_duration_seconds: $resume_b_duration
    },
    assertions: {
      rollback_storm_coverage: true,
      deterministic_recovery_behavior: true,
      no_unsafe_dispatch_escalation: true,
      no_unbounded_degradation: true
    },
    artifacts: {
      chaos_log: $chaos_log,
      tx_matrix_log: $tx_matrix_log,
      tx_observability_log: $observability_log,
      resume_run_a_log: $resume_a_log,
      resume_run_b_log: $resume_b_log
    },
    residual_risks: [
      "Chaos campaign is deterministic with fixed seeds and does not model fully random external faults.",
      "Cross-process network partitions remain covered by dedicated distributed-mode drills."
    ]
  }' > "${REPORT_FILE}"

emit_log "passed" "chaos_evidence_bundle" "artifact_generated" "none" \
  "${REPORT_FILE#${ROOT_DIR}/}" "generated chaos burn-in evidence bundle"

emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "chaos burn-in completed with rollback storm and deterministic recovery assertions"

echo "Mission chaos e2e passed."
echo "Structured logs: ${LOG_FILE#${ROOT_DIR}/}"
echo "Chaos report: ${REPORT_FILE#${ROOT_DIR}/}"
