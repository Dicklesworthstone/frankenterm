#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_so7qh_5_trauma_guard_config_tuning"
CORRELATION_ID="ft-so7qh.5-${RUN_ID}"
PANE_ID=1
TARGET_DIR="target-rch-ft-so7qh-5-${RUN_ID}"

LOG_FILE="${LOG_DIR}/ft_so7qh_5_${RUN_ID}.jsonl"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local command_input="$3"
  local decision_path="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local input_summary="$8"
  local ts
  local command_hash

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  command_hash="$(printf '%s' "${command_input}" | cksum | awk '{print $1}')"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "trauma_guard.config.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg pane_id "${PANE_ID}" \
    --arg command_hash "${command_hash}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg decision_reason "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      pane_id: ($pane_id | tonumber),
      command_hash: ($command_hash | tonumber),
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      decision_reason: $decision_reason,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

run_target_test() {
  local scenario="$1"
  local test_name="$2"
  local command_input="$3"
  local decision_path="$4"
  local success_reason="$5"

  local stdout_file="${LOG_DIR}/ft_so7qh_5_${RUN_ID}_${scenario}.stdout.log"

  emit_log \
    "running" \
    "${scenario}" \
    "${command_input}" \
    "cargo_test" \
    "none" \
    "none" \
    "$(basename "${stdout_file}")" \
    "Executing via shared rch guard: cargo test -p frankenterm-core --lib ${test_name} -- --nocapture"

  set +e
  run_rch_cargo_logged "${stdout_file}" env CARGO_TARGET_DIR="${TARGET_DIR}" \
    cargo test -p frankenterm-core --lib "${test_name}" -- --nocapture
  local status=$?
  set -e
  cat "${stdout_file}"

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${scenario}" \
      "${command_input}" \
      "cargo_test" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${stdout_file}")" \
      "test=${test_name} exit=${status}"
    return "${status}"
  fi

  if ! grep -q "${test_name} ... ok" "${stdout_file}"; then
    emit_log \
      "failed" \
      "${scenario}" \
      "${command_input}" \
      "assertion_check" \
      "unexpected_test_output" \
      "missing_success_marker" \
      "$(basename "${stdout_file}")" \
      "Expected success marker for ${test_name}"
    return 1
  fi

  emit_log \
    "passed" \
    "${scenario}" \
    "${command_input}" \
    "${decision_path}" \
    "${success_reason}" \
    "none" \
    "$(basename "${stdout_file}")" \
    "test=${test_name}"

  return 0
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured log emission and rch metadata artifacts" >&2
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "so7qh_5"
ensure_rch_ready

emit_log \
  "started" \
  "suite_init" \
  "cargo test -p frankenterm-core trauma guard config suite" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "scenarios=3"

run_target_test \
  "config_parses_and_hot_reload_diff" \
  "hot_reload_allows_trauma_guard_change" \
  "[safety.trauma_guard] tuning parse + diff" \
  "config.parse->hot_reload.diff" \
  "trauma_guard_hot_reload_detected"

run_target_test \
  "policy_disable_skips_block" \
  "command_gate_trauma_disabled_skips_trauma_block" \
  "FT_BYPASS_TRAUMA=0 git status" \
  "policy.authorize->trauma_disabled->allow" \
  "trauma_guard_disabled_allows"

run_target_test \
  "ingest_reload_thresholds" \
  "set_trauma_guard_config_reloads_thresholds" \
  "cargo test recurrence threshold update" \
  "ingest.registry->set_trauma_guard_config->record_command_result" \
  "runtime_reload_applied"

emit_log \
  "passed" \
  "suite_complete" \
  "ft-so7qh.5" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "scenarios=3"
