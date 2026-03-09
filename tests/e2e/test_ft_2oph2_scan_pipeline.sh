#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_2oph2_scan_pipeline"
CORRELATION_ID="ft-2oph2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  TARGET_DIR="${CARGO_TARGET_DIR}"
else
  TARGET_DIR="/tmp/target-rch-ft-2oph2-${RUN_ID}"
fi
LAST_STEP_LOG=""
RCH_LOCAL_FALLBACK_COUNT=0
BENCH_ENOSPC=0

emit_log() {
  local component="$1"
  local decision_path="$2"
  local input_summary="$3"
  local outcome="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "${component}" \
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

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "dependency_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  (
    cd "${ROOT_DIR}"
    "$@"
  ) 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return "${rc}"
}

record_rch_mode() {
  local decision_path="$1"
  if grep -q "\\[RCH\\] local" "${LAST_STEP_LOG}"; then
    RCH_LOCAL_FALLBACK_COUNT=$((RCH_LOCAL_FALLBACK_COUNT + 1))
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "degraded" "rch_fail_open_local_fallback" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_remote_offload" "none" "$(basename "${LAST_STEP_LOG}")"
  fi
}

run_rch_expect_success() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" env CARGO_TARGET_DIR="${TARGET_DIR}" rch exec -- "$@"; then
    record_rch_mode "${decision_path}"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "command_succeeded" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    record_rch_mode "${decision_path}"
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "command_failed" "CARGO-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

run_rch_expect_success_or_enospc_degraded() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  if [[ "${BENCH_ENOSPC}" -eq 1 ]]; then
    emit_log "validation" "${decision_path}" "${input_summary}" "degraded" "benchmark_skipped_due_enospc" "ENOSPC" "$(basename "${STDOUT_FILE}")"
    return 0
  fi

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" env CARGO_TARGET_DIR="${TARGET_DIR}" rch exec -- "$@"; then
    record_rch_mode "${decision_path}"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "command_succeeded" "none" "$(basename "${LAST_STEP_LOG}")"
    return 0
  fi

  record_rch_mode "${decision_path}"
  if rg -q "No space left on device" "${LAST_STEP_LOG}"; then
    BENCH_ENOSPC=1
    emit_log "validation" "${decision_path}" "${input_summary}" "degraded" "disk_full_enospc" "ENOSPC" "$(basename "${LAST_STEP_LOG}")"
    return 0
  fi

  emit_log "validation" "${decision_path}" "${input_summary}" "failed" "command_failed" "CARGO-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
}

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rch
require_cmd cargo

if run_step "rch_check" rch check; then
  check_log="${LAST_STEP_LOG}"
  emit_log "preflight" "rch_check" "health_check" "passed" "rch_ready" "none" "$(basename "${check_log}")"
else
  emit_log "preflight" "rch_check" "health_check" "degraded" "rch_check_degraded" "RCH-E100" "$(basename "${LAST_STEP_LOG}")"
fi

if run_step "rch_probe" rch workers probe --all --json; then
  probe_log="${LAST_STEP_LOG}"
  healthy_workers="$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${probe_log}" 2>/dev/null || echo 0)"
  if [[ "${healthy_workers}" -lt 1 ]]; then
    emit_log "preflight" "rch_probe" "workers_probe" "degraded" "rch_workers_unreachable_probe" "RCH-E101" "$(basename "${probe_log}")"
  else
    emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"
  fi
else
  emit_log "preflight" "rch_probe" "workers_probe" "degraded" "rch_probe_failed" "RCH-E101" "$(basename "${LAST_STEP_LOG}")"
fi

run_rch_expect_success \
  "nominal_batch_chunk_parity" \
  "nominal_path.scan_pipeline_alignment" \
  "cargo test -p frankenterm-core --lib batch_and_chunked_agree_on_line_aligned_chunks -- --nocapture" \
  cargo test -p frankenterm-core --lib batch_and_chunked_agree_on_line_aligned_chunks -- --nocapture

run_rch_expect_success \
  "simd_streaming_parity" \
  "nominal_path.simd_streaming_equivalence" \
  "cargo test -p frankenterm-core --test proptest_simd_scan streaming_scan_matches_full_scan_on_arbitrary_chunks -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_simd_scan streaming_scan_matches_full_scan_on_arbitrary_chunks -- --nocapture

run_rch_expect_success \
  "trigger_locate_count_consistency" \
  "nominal_path.pattern_trigger_consistency" \
  "cargo test -p frankenterm-core --test proptest_pattern_trigger locate_count_consistency -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_pattern_trigger locate_count_consistency -- --nocapture

run_rch_expect_success \
  "compression_roundtrip_medium" \
  "nominal_path.byte_compression_roundtrip" \
  "cargo test -p frankenterm-core --test proptest_byte_compression roundtrip_medium_payload -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_byte_compression roundtrip_medium_payload -- --nocapture

run_rch_expect_success \
  "pipeline_trigger_parity" \
  "nominal_path.scan_pipeline_trigger_parity" \
  "cargo test -p frankenterm-core --test proptest_scan_pipeline chunked_batch_trigger_parity -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_scan_pipeline chunked_batch_trigger_parity -- --nocapture

run_rch_expect_success \
  "split_pattern_recovery" \
  "recovery_path.chunk_split_trigger_overlap" \
  "cargo test -p frankenterm-core --lib chunked_recovers_split_patterns_with_overlap -- --nocapture" \
  cargo test -p frankenterm-core --lib chunked_recovers_split_patterns_with_overlap -- --nocapture

run_rch_expect_success \
  "recovery_cross_boundary_ansi" \
  "recovery_path.chunk_boundary_escape_state" \
  "cargo test -p frankenterm-core --lib chunked_pipeline_cross_boundary_ansi -- --nocapture" \
  cargo test -p frankenterm-core --lib chunked_pipeline_cross_boundary_ansi -- --nocapture

run_rch_expect_success_or_enospc_degraded \
  "bench_compile_simd_scan" \
  "benchmark_validation.simd_scan_no_run" \
  "cargo bench -p frankenterm-core --bench simd_scan --no-run" \
  cargo bench -p frankenterm-core --bench simd_scan --no-run

run_rch_expect_success_or_enospc_degraded \
  "bench_compile_pattern_trigger" \
  "benchmark_validation.pattern_trigger_no_run" \
  "cargo bench -p frankenterm-core --bench pattern_trigger --no-run" \
  cargo bench -p frankenterm-core --bench pattern_trigger --no-run

run_rch_expect_success_or_enospc_degraded \
  "bench_compile_scan_pipeline" \
  "benchmark_validation.scan_pipeline_no_run" \
  "cargo bench -p frankenterm-core --bench scan_pipeline --no-run" \
  cargo bench -p frankenterm-core --bench scan_pipeline --no-run

if [[ "${RCH_LOCAL_FALLBACK_COUNT}" -gt 0 ]]; then
  emit_log "summary" "scenario_complete" "scenario_complete_with_local_fallback" "degraded" "rch_fail_open_local_fallback" "none" "$(basename "${STDOUT_FILE}")"
else
  emit_log "summary" "scenario_complete" "scenario_complete_remote_offload" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
fi

echo "ft-2oph2 scan pipeline scenario complete. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
