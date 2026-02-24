#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_l5em3_1_spmc_ring_buffer"
CORRELATION_ID="ft-l5em3.1-${RUN_ID}"
PANE_ID=0
TARGET_DIR="target-rch-ft-l5em3-1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_l5em3_1_${RUN_ID}.jsonl"

emit_log() {
  local status="$1"
  local step="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local details="$7"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "aegis.spmc.e2e" \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg pane_id "${PANE_ID}" \
    --arg step "${step}" \
    --arg status "${status}" \
    --arg decision_path "${decision_path}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    --arg details "${details}" \
    '{
      timestamp: $timestamp,
      component: $component,
      run_id: $run_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      pane_id: ($pane_id | tonumber),
      step: $step,
      status: $status,
      decision_path: $decision_path,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      details: $details
    }' >> "${LOG_FILE}"
}

run_test_step() {
  local step="$1"
  local test_name="$2"
  local decision_path="$3"
  local success_reason="$4"

  local stdout_file="${LOG_DIR}/ft_l5em3_1_${RUN_ID}_${step}.stdout.log"
  local cmd=(
    env TMPDIR=/tmp
    rch exec --
    env CARGO_TARGET_DIR="${TARGET_DIR}"
    cargo test -p frankenterm-core --lib "${test_name}" -- --nocapture
  )

  emit_log \
    "running" \
    "${step}" \
    "cargo_test" \
    "none" \
    "none" \
    "$(basename "${stdout_file}")" \
    "Executing: ${cmd[*]}"

  set +e
  (
    cd "${ROOT_DIR}"
    "${cmd[@]}"
  ) 2>&1 | tee "${stdout_file}"
  local status=${PIPESTATUS[0]}
  set -e

  if grep -q "\\[RCH\\] local" "${stdout_file}"; then
    emit_log \
      "failed" \
      "${step}" \
      "offload_guard" \
      "rch_local_fallback" \
      "offload_policy_violation" \
      "$(basename "${stdout_file}")" \
      "rch fell back to local execution; refusing local CPU-heavy test run"
    return 1
  fi

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${step}" \
      "cargo_test" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${stdout_file}")" \
      "test=${test_name} exit=${status}"
    return "${status}"
  fi

  if ! grep -Eq "test .*${test_name} .*ok" "${stdout_file}"; then
    emit_log \
      "failed" \
      "${step}" \
      "assertion_check" \
      "missing_success_marker" \
      "unexpected_test_output" \
      "$(basename "${stdout_file}")" \
      "Expected success marker for ${test_name}"
    return 1
  fi

  emit_log \
    "passed" \
    "${step}" \
    "${decision_path}" \
    "${success_reason}" \
    "none" \
    "$(basename "${stdout_file}")" \
    "test=${test_name}"
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "steps=4"

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch must be available for offloaded cargo execution"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logs" >&2
  exit 1
fi

run_test_step \
  "backpressure_wait" \
  "spmc_send_waits_for_slowest_consumer" \
  "send->wait_on_full->consumer_unblocks" \
  "backpressure_enforced"

run_test_step \
  "backpressure_try_send" \
  "spmc_try_send_fails_when_any_consumer_is_full" \
  "try_send->full_queue->error" \
  "nonblocking_backpressure_signal"

run_test_step \
  "drain_and_close" \
  "spmc_close_allows_drain_then_none" \
  "close->drain->none" \
  "graceful_shutdown_semantics"

run_test_step \
  "runtime_handoff_checksum" \
  "runtime_spmc_handoff_preserves_exact_output_checksum" \
  "ingest->relay->persist->sha256_isomorphism" \
  "exact_output_fidelity"

emit_log \
  "passed" \
  "suite_complete" \
  "suite_complete" \
  "all_steps_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "steps=4"
