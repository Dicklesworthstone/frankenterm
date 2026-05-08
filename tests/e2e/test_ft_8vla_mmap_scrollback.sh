#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_8vla_mmap_scrollback"
CORRELATION_ID="ft-8vla-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-8vla-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
LAST_STEP_LOG=""

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "8vla_mmap_scrollback"

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

run_rch_expect_success() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  if run_rch_cargo_logged "${LAST_STEP_LOG}" env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"; then
    tee -a "${STDOUT_FILE}" < "${LAST_STEP_LOG}"
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_remote_offload" "none" "$(basename "${LAST_STEP_LOG}")"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "command_succeeded" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    tee -a "${STDOUT_FILE}" < "${LAST_STEP_LOG}"
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_remote_offload" "none" "$(basename "${LAST_STEP_LOG}")"
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "command_failed" "CARGO-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

if ! command -v jq >/dev/null 2>&1; then
  echo "missing required command: jq" >&2
  exit 1
fi

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"
: > "${STDOUT_FILE}"

require_cmd rg
require_cmd rch
require_cmd cargo

ensure_rch_ready
emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "$(rch_probe_log_path)")"
emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "passed" "remote_exec_confirmed" "none" "$(basename "$(rch_smoke_log_path)")"

run_rch_expect_success \
  "segment_line_round_trip" \
  "nominal_path.segment_encoding" \
  "cargo test -p frankenterm-core --lib mmap_segment_line_round_trip_preserves_multiline_content -- --nocapture" \
  cargo test -p frankenterm-core --lib mmap_segment_line_round_trip_preserves_multiline_content -- --nocapture

run_rch_expect_success \
  "mmap_decode_failure_injection" \
  "failure_injection.corrupted_mmap_line" \
  "cargo test -p frankenterm-core --lib get_segments_prefers_mmap_lane_and_falls_back_to_sqlite_on_decode_error -- --nocapture" \
  cargo test -p frankenterm-core --lib get_segments_prefers_mmap_lane_and_falls_back_to_sqlite_on_decode_error -- --nocapture

run_rch_expect_success \
  "recovery_truncated_offsets" \
  "recovery_path.sqlite_tail_fallback" \
  "cargo test -p frankenterm-core --test proptest_mmap store_falls_back_to_sqlite_when_mmap_tail_offsets_are_invalidated -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_mmap store_falls_back_to_sqlite_when_mmap_tail_offsets_are_invalidated -- --nocapture

run_rch_expect_success \
  "recovery_unwritable_log_path" \
  "recovery_path.sqlite_write_fallback" \
  "cargo test -p frankenterm-core --test proptest_mmap store_falls_back_to_sqlite_when_log_path_is_unwritable -- --nocapture" \
  cargo test -p frankenterm-core --test proptest_mmap store_falls_back_to_sqlite_when_log_path_is_unwritable -- --nocapture

run_rch_expect_success \
  "benchmark_compile_contract" \
  "benchmark_validation.no_run_compile" \
  "cargo check -p frankenterm-core --bench mmap_scrollback --message-format short" \
  cargo check -p frankenterm-core --bench mmap_scrollback --message-format short

emit_log "summary" "scenario_complete" "scenario_complete_remote_offload" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"

echo "ft-8vla mmap scrollback scenario complete. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
