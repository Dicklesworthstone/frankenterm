#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_l5em3_2_simd_scan"
CORRELATION_ID="ft-l5em3.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_l5em3_2_${RUN_ID}.jsonl"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-l5em3-2-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

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
    --arg component "aegis.simd_scan.e2e" \
    --arg run_id "${RUN_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
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
      step: $step,
      status: $status,
      decision_path: $decision_path,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      details: $details
    }' >> "${LOG_FILE}"
}

run_step() {
  local step="$1"
  local decision_path="$2"
  local success_reason="$3"
  shift 3
  local cmd=("$@")
  local stdout_file="${LOG_DIR}/ft_l5em3_2_${RUN_ID}_${step}.stdout.log"

  emit_log \
    "running" \
    "${step}" \
    "${decision_path}" \
    "none" \
    "none" \
    "$(basename "${stdout_file}")" \
    "Executing via rch: env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${cmd[*]}"

  set +e
  run_rch_cargo_logged "${stdout_file}" \
    env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "${cmd[@]}"
  local status=$?
  set -e

  if grep -q "\\[RCH\\] local" "${stdout_file}"; then
    emit_log \
      "failed" \
      "${step}" \
      "offload_guard" \
      "rch_local_fallback" \
      "offload_policy_violation" \
      "$(basename "${stdout_file}")" \
      "rch fell back to local execution; refusing local CPU-heavy run"
    return 1
  fi

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${step}" \
      "${decision_path}" \
      "command_failed" \
      "cargo_command_failed" \
      "$(basename "${stdout_file}")" \
      "exit=${status}"
    return "${status}"
  fi

  emit_log \
    "passed" \
    "${step}" \
    "${decision_path}" \
    "${success_reason}" \
    "none" \
    "$(basename "${stdout_file}")" \
    "ok"
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logs" >&2
  exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "l5em3_2"

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "steps=8"

ensure_rch_ready

emit_log \
  "passed" \
  "rch_preflight" \
  "remote_execution_ready" \
  "rch_shared_guard_passed" \
  "none" \
  "$(basename "$(rch_probe_log_path)")" \
  "shared rch guard reported reachable workers"

emit_log \
  "passed" \
  "rch_smoke" \
  "remote_execution_ready" \
  "rch_remote_smoke_passed" \
  "none" \
  "$(basename "$(rch_smoke_log_path)")" \
  "shared rch guard verified fail-closed remote cargo execution"

run_step \
  "escape_boundary_unit" \
  "stateful_unit_escape_boundary" \
  "escape_carry_state_preserved" \
  cargo test -p frankenterm-core --lib stateful_scan_tracks_escape_across_chunk_boundary -- --nocapture

run_step \
  "utf8_boundary_unit" \
  "stateful_unit_utf8_boundary" \
  "utf8_carry_state_preserved" \
  cargo test -p frankenterm-core --lib stateful_scan_tracks_partial_utf8_across_chunks -- --nocapture

run_step \
  "streaming_split_proptest" \
  "proptest_streaming_equivalence" \
  "streaming_equals_full_scan" \
  cargo test -p frankenterm-core --test proptest_simd_scan streaming_scan_matches_full_scan_on_arbitrary_chunks -- --nocapture

run_step \
  "ansi_split_proptest" \
  "proptest_ansi_boundary_equivalence" \
  "ansi_split_metrics_preserved" \
  cargo test -p frankenterm-core --test proptest_simd_scan ansi_boundary_splits_preserve_metrics -- --nocapture

run_step \
  "stateful_parity_proptest" \
  "proptest_stateful_fast_vs_scalar_parity" \
  "stateful_fast_path_matches_scalar" \
  cargo test -p frankenterm-core --lib stateful_fast_path_matches_scalar_for_random_bytes_and_state -- --nocapture

run_step \
  "bocpd_chunk_escape_state" \
  "bocpd_chunked_scan_state_carry" \
  "bocpd_chunk_observe_preserves_escape_carry" \
  cargo test -p frankenterm-core --lib pane_bocpd_observe_text_chunk_preserves_escape_state_across_chunks -- --nocapture

run_step \
  "bocpd_manager_chunk_auto_register" \
  "bocpd_manager_chunked_auto_register" \
  "bocpd_manager_observe_text_chunk_works" \
  cargo test -p frankenterm-core --lib manager_observe_text_chunk_auto_registers_and_uses_scan_carry -- --nocapture

run_step \
  "dense_logs_benchmark" \
  "criterion_dense_logs_throughput" \
  "benchmark_collected" \
  cargo bench -p frankenterm-core --bench simd_scan -- \
  simd_scan_chunked_stateful --sample-size 10 --warm-up-time 0.2 --measurement-time 0.5 --noplot

emit_log \
  "passed" \
  "suite_complete" \
  "suite_complete" \
  "all_steps_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "steps=8"

echo "Scenario: ${SCENARIO_ID}"
echo "Logs: tests/e2e/logs/$(basename "${LOG_FILE}")"
