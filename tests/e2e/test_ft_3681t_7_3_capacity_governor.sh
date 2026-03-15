#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_7_3_capacity_governor"
CORRELATION_ID="ft-3681t.7.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_3681t_7_3_capacity_governor_${RUN_ID}.jsonl"
TARGET_DIR="target-rch-ft-3681t-7-3"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "3681t_7_3_capacity_governor"
ensure_rch_ready

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "ft_3681t_7_3.capacity_governor.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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

run_step() {
  local scenario="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "running" "${scenario}" "${decision_path}" "none" "none" "$(basename "${LOG_FILE}")" "${input_summary}"
  if "$@"; then
    emit_log "passed" "${scenario}" "${decision_path}" "step_passed" "none" "$(basename "${LOG_FILE}")" "${input_summary}"
    return 0
  fi

  emit_log "failed" "${scenario}" "${decision_path}" "step_failed" "command_failed" "$(basename "${LOG_FILE}")" "${input_summary}"
  return 1
}

run_rch_step() {
  local scenario="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  local tmp_output
  tmp_output="$(mktemp)"
  emit_log "running" "${scenario}" "${decision_path}" "none" "none" "$(basename "${LOG_FILE}")" "${input_summary}"

  if ! "$@" >"${tmp_output}" 2>&1; then
    cat "${tmp_output}" >> "${LOG_FILE}"
    emit_log "failed" "${scenario}" "${decision_path}" "rch_command_failed" "command_failed" "$(basename "${LOG_FILE}")" "${input_summary}"
    rm -f "${tmp_output}"
    return 1
  fi

  cat "${tmp_output}" >> "${LOG_FILE}"
  if rg -q '^\[RCH\] local' "${tmp_output}"; then
    emit_log "failed" "${scenario}" "${decision_path}" "rch_fell_open_local" "rch_local_fallback" "$(basename "${LOG_FILE}")" "${input_summary}"
    rm -f "${tmp_output}"
    return 1
  fi

  emit_log "passed" "${scenario}" "${decision_path}" "step_passed" "none" "$(basename "${LOG_FILE}")" "${input_summary}"
  rm -f "${tmp_output}"
  return 0
}

emit_log "started" "suite_init" "script_init" "none" "none" "$(basename "${LOG_FILE}")" "ft-3681t.7.3 capacity governor validation"

for tool in jq rg rch cargo; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    emit_log "failed" "suite_init" "preflight_tools" "missing_tool" "${tool}_not_found" "$(basename "${LOG_FILE}")" "${tool} is required"
    exit 1
  fi
done

run_rch_step \
  "rch_remote_smoke" \
  "preflight_remote_exec" \
  "rch exec -- cargo check --help" \
  rch exec -- cargo check --help

run_rch_step \
  "unit_zero_workers" \
  "cargo_test_lib" \
  "rch exec -- cargo test -p frankenterm-core --lib zero_rch_workers_throttle_instead_of_offload_at_concurrency_limit -- --exact --nocapture" \
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" cargo test -p frankenterm-core --lib zero_rch_workers_throttle_instead_of_offload_at_concurrency_limit -- --exact --nocapture

run_rch_step \
  "integration_policy_and_offload" \
  "cargo_test_integration" \
  "rch exec -- cargo test -p frankenterm-core --test capacity_governor_integration -- --nocapture" \
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" cargo test -p frankenterm-core --test capacity_governor_integration -- --nocapture

run_rch_step \
  "proptest_zero_workers" \
  "cargo_test_proptest" \
  "rch exec -- cargo test -p frankenterm-core --test proptest_capacity_governor zero_workers_prevent_rch_offload_even_when_rch_flagged -- --exact --nocapture" \
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" cargo test -p frankenterm-core --test proptest_capacity_governor zero_workers_prevent_rch_offload_even_when_rch_flagged -- --exact --nocapture

emit_log "passed" "suite_complete" "script_complete" "all_steps_passed" "none" "$(basename "${LOG_FILE}")" "ft-3681t.7.3 capacity governor validation complete"
printf 'wrote structured log: %s\n' "${LOG_FILE}"
