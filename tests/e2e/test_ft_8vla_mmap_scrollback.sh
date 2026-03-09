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
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/target-rch-ft-8vla}-${RUN_ID}"
LAST_STEP_LOG=""
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_SOCKET_PATH_REGEX='unix_listener: path .*too long for Unix domain socket|too long for Unix domain socket'
LOCAL_RCH_TMPDIR_OVERRIDE=""

if [[ "$(uname -s)" == "Darwin" ]]; then
  LOCAL_RCH_TMPDIR_OVERRIDE="/tmp"
fi

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

rch_fail_open_detected() {
  local log_path="$1"
  grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${log_path}"
}

rch_socket_path_issue_detected() {
  local log_path="$1"
  grep -Eq "${RCH_SOCKET_PATH_REGEX}" "${log_path}"
}

healthy_workers_from_probe_log() {
  local log_path="$1"
  awk 'BEGIN { capture = 0 } /^[[:space:]]*\{/ { capture = 1 } capture { print }' "${log_path}" \
    | jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' 2>/dev/null \
    || echo 0
}

run_rch() {
  if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
    TMPDIR="${LOCAL_RCH_TMPDIR_OVERRIDE}" rch "$@"
  else
    rch "$@"
  fi
}

ensure_rch_remote_only() {
  local decision_path="$1"
  local input_summary="$2"
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    if rch_socket_path_issue_detected "${LAST_STEP_LOG}"; then
      emit_log "validation" "${decision_path}" "${input_summary}" "failed" "rch_local_socket_path_too_long" "RCH-LOCAL-TMPDIR" "$(basename "${LAST_STEP_LOG}")"
    else
      emit_log "validation" "${decision_path}" "${input_summary}" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    fi
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
  emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_remote_offload" "none" "$(basename "${LAST_STEP_LOG}")"
}

run_rch_expect_success() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" run_rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"; then
    ensure_rch_remote_only "${decision_path}" "${input_summary}"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "command_succeeded" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    ensure_rch_remote_only "${decision_path}" "${input_summary}"
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "command_failed" "CARGO-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rg
require_cmd rch
require_cmd cargo

if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
  emit_log "preflight" "rch_local_tmpdir_workaround" "TMPDIR=${LOCAL_RCH_TMPDIR_OVERRIDE}" "applied" "darwin_controlmaster_socket_guard" "none" "$(basename "${STDOUT_FILE}")"
fi

if run_step "rch_check" run_rch check; then
  check_log="${LAST_STEP_LOG}"
  emit_log "preflight" "rch_check" "health_check" "passed" "rch_ready" "none" "$(basename "${check_log}")"
else
  emit_log "preflight" "rch_check" "health_check" "failed" "rch_check_failed" "RCH-E100" "$(basename "${LAST_STEP_LOG}")"
  exit 2
fi

if run_step "rch_probe" run_rch workers probe --all --json; then
  probe_log="${LAST_STEP_LOG}"
  healthy_workers="$(healthy_workers_from_probe_log "${probe_log}")"
  if [[ "${healthy_workers}" -lt 1 ]]; then
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_workers_unreachable_probe" "RCH-E101" "$(basename "${probe_log}")"
    echo "no reachable rch workers; refusing local fallback" >&2
    exit 2
  else
    emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"
  fi
else
  if rch_socket_path_issue_detected "${LAST_STEP_LOG}"; then
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_local_socket_path_too_long" "RCH-LOCAL-TMPDIR" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_probe_failed" "RCH-E101" "$(basename "${LAST_STEP_LOG}")"
  fi
  exit 2
fi

emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step "rch_remote_smoke" run_rch exec -- cargo check --help; then
  ensure_rch_remote_only "rch_remote_smoke" "cargo check --help"
  emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "passed" "remote_exec_confirmed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  ensure_rch_remote_only "rch_remote_smoke" "cargo check --help"
  emit_log "preflight" "rch_remote_smoke" "cargo_check_help" "failed" "rch_remote_smoke_failed" "RCH-E102" "$(basename "${LAST_STEP_LOG}")"
  exit 2
fi

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
