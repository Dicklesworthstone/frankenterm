#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-hme39.2"
SCENARIO_ID="ft_hme39_2_mux_loopback"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/logs/terminal-conformance/${BEAD_ID}/${RUN_ID}"
EVENT_LOG="${ARTIFACT_DIR}/events.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
REMOTE_TARGET_DIR="${RCH_REMOTE_TARGET_DIR:-target/rch-e2e-ft-hme39-2-${RUN_ID}}"
MUX_BIN="\${CARGO_TARGET_DIR}/debug/frankenterm-mux-server"

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-1800}"
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"

mkdir -p "${ARTIFACT_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_hme39_2_mux_loopback" "${ROOT_DIR}"

now_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    jq -cn \
      --arg timestamp "$(now_ts)" \
      --arg component "terminal_conformance.e2e" \
      --arg scenario_id "${SCENARIO_ID}" \
      --arg correlation_id "${RUN_ID}" \
      --arg decision_path "preflight.${cmd}" \
      --arg input_summary "missing command ${cmd}" \
      --arg outcome "failed" \
      --arg reason_code "missing_prerequisite" \
      --arg error_code "E2E-PREREQ" \
      --arg artifact_path "${EVENT_LOG#"${ROOT_DIR}"/}" \
      '{timestamp:$timestamp,component:$component,scenario_id:$scenario_id,correlation_id:$correlation_id,decision_path:$decision_path,input_summary:$input_summary,outcome:$outcome,reason_code:$reason_code,error_code:$error_code,artifact_path:$artifact_path}' \
      >>"${EVENT_LOG}"
    exit 1
  fi
}

emit_event() {
  local decision_path="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local worker="${7:-}"

  jq -cn \
    --arg timestamp "$(now_ts)" \
    --arg component "terminal_conformance.e2e" \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${RUN_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path#"${ROOT_DIR}"/}" \
    --arg worker "${worker}" \
    '{
      timestamp: $timestamp,
      component: $component,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    } + (if $worker == "" then {} else {rch_worker_id: $worker} end)' \
    >>"${EVENT_LOG}"
}

worker_for_log() {
  local log_file="$1"
  local meta_file
  meta_file="$(rch_log_meta_path "${log_file}")"
  if [[ -f "${meta_file}" ]]; then
    jq -r '.selected_worker // ""' "${meta_file}" 2>/dev/null || true
  fi
}

failure_reason_for_log() {
  local log_file="$1"
  local meta_file code
  meta_file="$(rch_log_meta_path "${log_file}")"
  if [[ ! -f "${meta_file}" ]]; then
    printf '%s\n' "source_or_test_failure"
    return
  fi
  if jq -e '.fail_open_detected == true' "${meta_file}" >/dev/null 2>&1; then
    printf '%s\n' "rch_local_fallback"
    return
  fi
  if jq -e '.timed_out == true' "${meta_file}" >/dev/null 2>&1; then
    printf '%s\n' "rch_remote_timeout"
    return
  fi
  code="$(jq -r '.failure_reason_code // ""' "${meta_file}" 2>/dev/null || true)"
  if [[ -n "${code}" ]]; then
    printf 'rch_infrastructure_%s\n' "${code}"
    return
  fi
  printf '%s\n' "source_or_test_failure"
}

run_rch_step() {
  local decision_path="$1"
  local log_file="$2"
  local command_summary="$3"
  shift 3

  emit_event "${decision_path}" "running" "remote_rch_started" "none" "${log_file}" "${command_summary}"
  set +e
  run_rch_cargo_logged "${log_file}" "$@"
  local rc=$?
  set -e

  local worker
  worker="$(worker_for_log "${log_file}")"
  if [[ "${rc}" -eq 0 ]]; then
    emit_event "${decision_path}" "passed" "remote_rch_passed" "none" "${log_file}" "${command_summary}" "${worker}"
    return 0
  fi

  local reason
  reason="$(failure_reason_for_log "${log_file}")"
  emit_event "${decision_path}" "failed" "${reason}" "cargo_or_rch_failed" "${log_file}" "${command_summary}" "${worker}"
  return "${rc}"
}

write_summary() {
  local outcome="$1"
  local build_log="$2"
  local test_log="$3"

  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg outcome "${outcome}" \
    --arg artifact_dir "${ARTIFACT_DIR#"${ROOT_DIR}"/}" \
    --arg event_log "${EVENT_LOG#"${ROOT_DIR}"/}" \
    --arg summary_file "${SUMMARY_FILE#"${ROOT_DIR}"/}" \
    --arg build_log "${build_log#"${ROOT_DIR}"/}" \
    --arg build_meta "$(rch_log_meta_path "${build_log}" | sed "s#^${ROOT_DIR}/##")" \
    --arg test_log "${test_log#"${ROOT_DIR}"/}" \
    --arg test_meta "$(rch_log_meta_path "${test_log}" | sed "s#^${ROOT_DIR}/##")" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --arg mux_bin "${MUX_BIN}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      remote_target_dir: $remote_target_dir,
      mux_bin: $mux_bin,
      artifacts: {
        events_jsonl: $event_log,
        summary_json: $summary_file,
        mux_server_build_log: $build_log,
        mux_server_build_meta: $build_meta,
        loopback_test_log: $test_log,
        loopback_test_meta: $test_meta
      }
    }' >"${SUMMARY_FILE}"
}

require_cmd jq
require_cmd rch

emit_event "suite.start" "running" "suite_started" "none" "${EVENT_LOG}" \
  "artifact_dir=${ARTIFACT_DIR#"${ROOT_DIR}"/}; remote_target_dir=${REMOTE_TARGET_DIR}"
ensure_rch_ready
emit_event "preflight.rch" "passed" "rch_ready" "none" "$(rch_probe_log_path)" \
  "RCH remote workers reachable; smoke preflight skipped=${RCH_SKIP_SMOKE_PREFLIGHT}"

BUILD_LOG="${ARTIFACT_DIR}/loopback_test.log"
TEST_LOG="${BUILD_LOG}"
status=0

if ! run_rch_step "loopback.spawn_send_resize_read" "${TEST_LOG}" \
  "cargo test -p frankenterm-core --no-default-features --features vendored,asupersync-runtime --test snapshot_real_mux no_mock_spawn_send_resize_read_loopback" \
  env FT_REAL_WEZTERM_TESTS=1 \
  CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
  cargo test -p frankenterm-core --no-default-features --features vendored,asupersync-runtime \
    --test snapshot_real_mux no_mock_spawn_send_resize_read_loopback -- --nocapture; then
  status=1
fi

if [[ "${status}" -eq 0 ]]; then
  emit_event "suite.complete" "passed" "all_assertions_satisfied" "none" "${SUMMARY_FILE}" \
    "no-mock spawn/send/resize/read loopback passed"
  write_summary "passed" "${BUILD_LOG}" "${TEST_LOG}"
else
  emit_event "suite.complete" "failed" "loopback_harness_failed" "E2E-TERMINAL-CONFORMANCE" "${SUMMARY_FILE}" \
    "see step logs and rch metadata"
  write_summary "failed" "${BUILD_LOG}" "${TEST_LOG}"
fi

printf 'summary=%s\n' "${SUMMARY_FILE#"${ROOT_DIR}"/}"
exit "${status}"
