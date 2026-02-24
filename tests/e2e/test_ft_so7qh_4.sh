#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_so7qh_4_synthetic_feedback_injection"
CORRELATION_ID="ft-so7qh.4-${RUN_ID}"
PANE_ID=1
TARGET_DIR="target-rch-ft-so7qh-4-${RUN_ID}"

LOG_FILE="${LOG_DIR}/ft_so7qh_4_${RUN_ID}.jsonl"

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
    --arg component "trauma_guard.e2e" \
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

  local stdout_file="${LOG_DIR}/ft_so7qh_4_${RUN_ID}_${scenario}.stdout.log"
  local test_cmd=(
    env TMPDIR=/tmp
    rch exec --
    env CARGO_TARGET_DIR="${TARGET_DIR}"
    cargo test -p frankenterm-core --lib "${test_name}" -- --nocapture
  )

  emit_log \
    "running" \
    "${scenario}" \
    "${command_input}" \
    "cargo_test" \
    "none" \
    "none" \
    "$(basename "${stdout_file}")" \
    "Executing: ${test_cmd[*]}"

  set +e
  (
    cd "${ROOT_DIR}"
    "${test_cmd[@]}"
  ) 2>&1 | tee "${stdout_file}"
  local status=${PIPESTATUS[0]}
  set -e

  if grep -q "\\[RCH\\] local" "${stdout_file}"; then
    emit_log \
      "failed" \
      "${scenario}" \
      "${command_input}" \
      "offload_guard" \
      "rch_local_fallback" \
      "remote_offload_required" \
      "$(basename "${stdout_file}")" \
      "rch fell back to local execution; refusing CPU-intensive local run"
    return 1
  fi

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

emit_log \
  "started" \
  "suite_init" \
  "cargo test -p frankenterm-core policy trauma feedback suite" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "scenarios=2"

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "rch exec -- cargo test ..." \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch must be installed for offloaded cargo execution"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "jq --version" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for structured log emission and worker probe parsing"
  exit 1
fi

if ! rch workers probe --all --json \
  | jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    >/dev/null; then
  emit_log \
    "failed" \
    "suite_init" \
    "rch workers probe --all --json" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${LOG_FILE}")" \
    "No reachable rch workers; aborting before cargo fallback can run locally"
  exit 1
fi

run_target_test \
  "trauma_deny_injects_feedback" \
  "e2e_trauma_guard_deny_injects_synthetic_feedback" \
  "cargo test -p core" \
  "authorize->deny(policy.trauma_guard.loop_block)->inject_synthetic_feedback" \
  "synthetic_feedback_injected"

run_target_test \
  "non_trauma_deny_no_feedback" \
  "e2e_non_trauma_deny_does_not_inject_synthetic_feedback" \
  "cat /etc/shadow" \
  "authorize->deny(non_trauma_rule)->no_synthetic_feedback" \
  "non_trauma_deny_skips_feedback"

emit_log \
  "passed" \
  "suite_complete" \
  "ft-so7qh.4" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "scenarios=2"
