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
SUMMARY_REMOTE_TARGET_DIR="${REMOTE_TARGET_DIR}"
MUX_BIN="\${CARGO_TARGET_DIR}/debug/frankenterm-mux-server"

RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-7200}"
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"
RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-1}"
export RCH_QUEUE_WHEN_BUSY="${RCH_QUEUE_WHEN_BUSY:-1}"
export RCH_BUILD_SLOTS="${RCH_BUILD_SLOTS:-2}"
export RCH_TEST_SLOTS="${RCH_TEST_SLOTS:-2}"
RCH_MIRROR_REQUIRED_PATHS="${RCH_MIRROR_REQUIRED_PATHS:-Cargo.toml,crates/frankenterm-core-replay/src/lib.rs,crates/frankenterm-core/src/lib.rs,crates/frankenterm-core/src/wezterm.rs,crates/frankenterm-core/src/vendored/mux_client.rs,crates/frankenterm-core/src/vendored/mux_pool.rs,crates/frankenterm-core/tests/snapshot_real_mux.rs,crates/frankenterm-core/tests/common/wezterm_subprocess.rs}"

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
  if sed -n 's/^error: could not parse\/generate dep info at: //p' "${log_file}" | grep -q . \
    && grep -Fq 'No such file or directory (os error 2)' "${log_file}"; then
    printf '%s\n' "rch_infrastructure_cargo_dep_info_missing"
    return
  fi
  if grep -Fq 'error: failed to load source for dependency' "${log_file}" 2>/dev/null \
    && grep -Eq 'failed to create (temporary|locked) file' "${log_file}" 2>/dev/null; then
    printf '%s\n' "rch_infrastructure_cargo_git_fetch_tempdir"
    return
  fi
  if grep -Fq '[RCH] local (no workers with Rust installed)' "${log_file}" 2>/dev/null; then
    printf '%s\n' "rch_infrastructure_no_rust_worker_available"
    return
  fi
  if ! rch_log_has_remote_execution_marker "${log_file}"; then
    printf '%s\n' "rch_infrastructure_remote_execution_missing"
    return
  fi
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

run_selection_preflight() {
  local log_file="$1"
  local target_dir="$2"
  local worker reason

  set +e
  run_rch --json diagnose \
    env \
    CARGO_TARGET_DIR="${target_dir}" \
    cargo build --config net.git-fetch-with-cli=true -p frankenterm-mux-server \
    >"${log_file}" 2>&1
  local rc=$?
  set -e

  rch_write_meta_json "${log_file}" "${rc}"

  if [[ "${rc}" -ne 0 ]]; then
    emit_event "preflight.rch_selection" "failed" "rch_selection_diagnose_failed" \
      "RCH-SELECTION-PREFLIGHT" "${log_file}" \
      "diagnose failed for cargo build -p frankenterm-mux-server target_dir=${target_dir}"
    return "${rc}"
  fi

  if ! jq -e . "${log_file}" >/dev/null 2>&1; then
    emit_event "preflight.rch_selection" "failed" "rch_selection_diagnose_invalid_json" \
      "RCH-SELECTION-PREFLIGHT" "${log_file}" \
      "diagnose did not emit valid JSON for cargo build -p frankenterm-mux-server target_dir=${target_dir}"
    return 1
  fi

  worker="$(jq -r '.data.worker_selection.worker.id // ""' "${log_file}" 2>/dev/null || true)"
  if [[ -n "${worker}" ]]; then
    emit_event "preflight.rch_selection" "passed" "rch_selection_ready" "none" "${log_file}" \
      "worker selection ready for cargo build -p frankenterm-mux-server target_dir=${target_dir}" \
      "${worker}"
    return 0
  fi

  local capability_log capability_source=""
  for capability_log in "$(rch_capabilities_log_path)" "$(rch_capabilities_refresh_log_path)"; do
    if [[ -f "${capability_log}" ]] \
      && jq -e '(.data.workers // .workers // []) | any(.capabilities.rustc_version? != null)' \
        "${capability_log}" >/dev/null 2>&1; then
      capability_source="${capability_log}"
      break
    fi
  done

  reason="$(jq -c '.data.worker_selection.reason // "unknown"' "${log_file}" 2>/dev/null || printf '%s' '"unknown"')"
  local capability_summary=""
  if [[ -n "${capability_source}" ]]; then
    capability_summary="; Rust capability source=${capability_source#"${ROOT_DIR}"/}"
  fi
  emit_event "preflight.rch_selection" "failed" "rch_selection_no_worker" \
    "RCH-SELECTION-PREFLIGHT" "${log_file}" \
    "daemon selection rejected cargo build -p frankenterm-mux-server target_dir=${target_dir}; reason=${reason}${capability_summary}"
  return 1
}

run_rch_step() {
  local decision_path="$1"
  local log_file="$2"
  local command_summary="$3"
  shift 3

  emit_event "${decision_path}" "running" "remote_rch_started" "none" "${log_file}" "${command_summary}"
  set +e
  (
    run_rch_cargo_logged "${log_file}" "$@"
  )
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
  local selection_log="${4:-}"
  local build_meta=""
  local test_meta=""
  local selection_meta=""

  if [[ -n "${build_log}" ]]; then
    build_meta="$(rch_log_meta_path "${build_log}" | sed "s#^${ROOT_DIR}/##")"
  fi
  if [[ -n "${test_log}" ]]; then
    test_meta="$(rch_log_meta_path "${test_log}" | sed "s#^${ROOT_DIR}/##")"
  fi
  if [[ -n "${selection_log}" ]]; then
    selection_meta="$(rch_log_meta_path "${selection_log}" | sed "s#^${ROOT_DIR}/##")"
  fi

  jq -cn \
    --arg bead_id "${BEAD_ID}" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg outcome "${outcome}" \
    --arg artifact_dir "${ARTIFACT_DIR#"${ROOT_DIR}"/}" \
    --arg event_log "${EVENT_LOG#"${ROOT_DIR}"/}" \
    --arg summary_file "${SUMMARY_FILE#"${ROOT_DIR}"/}" \
    --arg build_log "${build_log#"${ROOT_DIR}"/}" \
    --arg build_meta "${build_meta}" \
    --arg test_log "${test_log#"${ROOT_DIR}"/}" \
    --arg test_meta "${test_meta}" \
    --arg selection_log "${selection_log#"${ROOT_DIR}"/}" \
    --arg selection_meta "${selection_meta}" \
    --arg remote_target_dir "${SUMMARY_REMOTE_TARGET_DIR}" \
    --arg mux_bin "${MUX_BIN}" \
    '{
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      run_id: $run_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      remote_target_dir: $remote_target_dir,
      mux_bin: $mux_bin,
      artifacts: ({
        events_jsonl: $event_log,
        summary_json: $summary_file
      }
      + (if $build_log == "" then {} else {
        mux_server_build_log: $build_log,
        mux_server_build_meta: $build_meta
      } end)
      + (if $test_log == "" then {} else {
        loopback_test_log: $test_log,
        loopback_test_meta: $test_meta
      } end)
      + (if $selection_log == "" then {} else {
        rch_selection_preflight_log: $selection_log,
        rch_selection_preflight_meta: $selection_meta
      } end))
    }' >"${SUMMARY_FILE}"
}

run_mux_server_build() {
  local decision_path="$1"
  local log_file="$2"
  local target_dir="$3"

  run_rch_step "${decision_path}" "${log_file}" \
    "cargo build -p frankenterm-mux-server target_dir=${target_dir}" \
    env \
    CARGO_TARGET_DIR="${target_dir}" \
    cargo build --config net.git-fetch-with-cli=true -p frankenterm-mux-server
}

run_loopback_test() {
  local decision_path="$1"
  local log_file="$2"
  local target_dir="$3"

  run_rch_step "${decision_path}" "${log_file}" \
    "cargo test -p frankenterm-core --no-default-features --features vendored,asupersync-runtime --test snapshot_real_mux no_mock_spawn_send_resize_read_loopback target_dir=${target_dir}" \
    env FT_REAL_WEZTERM_TESTS=1 \
    CARGO_TARGET_DIR="${target_dir}" \
    cargo test --config net.git-fetch-with-cli=true \
      -p frankenterm-core --no-default-features --features vendored,asupersync-runtime \
      --test snapshot_real_mux no_mock_spawn_send_resize_read_loopback -- --nocapture
}

require_cmd jq
require_cmd rch

emit_event "suite.start" "running" "suite_started" "none" "${EVENT_LOG}" \
  "artifact_dir=${ARTIFACT_DIR#"${ROOT_DIR}"/}; remote_target_dir=${REMOTE_TARGET_DIR}"
ensure_rch_ready
emit_event "preflight.rch" "passed" "rch_ready" "none" "$(rch_probe_log_path)" \
  "RCH remote workers reachable; smoke preflight skipped=${RCH_SKIP_SMOKE_PREFLIGHT}"

SELECTION_LOG="${ARTIFACT_DIR}/rch_selection_preflight.log"
BUILD_LOG="${ARTIFACT_DIR}/mux_server_build.log"
TEST_LOG="${ARTIFACT_DIR}/loopback_test.log"
status=0
failed_log=""

if ! run_selection_preflight "${SELECTION_LOG}" "${REMOTE_TARGET_DIR}"; then
  status=1
  failed_log="${SELECTION_LOG}"
  BUILD_LOG=""
  TEST_LOG=""
elif ! run_mux_server_build "loopback.mux_server_build" "${BUILD_LOG}" "${REMOTE_TARGET_DIR}"; then
  status=1
  failed_log="${BUILD_LOG}"
elif ! run_loopback_test "loopback.spawn_send_resize_read" "${TEST_LOG}" "${REMOTE_TARGET_DIR}"; then
  status=1
  failed_log="${TEST_LOG}"
fi

if [[ "${status}" -ne 0 ]]; then
  failed_reason="$(failure_reason_for_log "${failed_log}")"
  if [[ "${failed_reason}" =~ ^rch_infrastructure_cargo_(dep_info_missing|git_fetch_tempdir)$ ]]; then
    RETRY_BUILD_LOG="${ARTIFACT_DIR}/mux_server_build.retry.log"
    RETRY_TEST_LOG="${ARTIFACT_DIR}/loopback_test.retry.log"
    RETRY_TARGET_DIR="${REMOTE_TARGET_DIR}-retry"
    BUILD_LOG="${RETRY_BUILD_LOG}"
    TEST_LOG="${RETRY_TEST_LOG}"
    SUMMARY_REMOTE_TARGET_DIR="${RETRY_TARGET_DIR}"
    status=0
    if ! run_mux_server_build "loopback.mux_server_build.retry_after_cargo_infra" \
      "${RETRY_BUILD_LOG}" "${RETRY_TARGET_DIR}"; then
      status=1
      failed_log="${RETRY_BUILD_LOG}"
    elif ! run_loopback_test "loopback.spawn_send_resize_read.retry_after_cargo_infra" \
      "${RETRY_TEST_LOG}" "${RETRY_TARGET_DIR}"; then
      status=1
      failed_log="${RETRY_TEST_LOG}"
    fi
  fi
fi

if [[ "${status}" -eq 0 ]]; then
  emit_event "suite.complete" "passed" "all_assertions_satisfied" "none" "${SUMMARY_FILE}" \
    "no-mock spawn/send/resize/read loopback passed"
  write_summary "passed" "${BUILD_LOG}" "${TEST_LOG}" "${SELECTION_LOG}"
else
  emit_event "suite.complete" "failed" "loopback_harness_failed" "E2E-TERMINAL-CONFORMANCE" "${SUMMARY_FILE}" \
    "see step logs and rch metadata"
  write_summary "failed" "${BUILD_LOG}" "${TEST_LOG}" "${SELECTION_LOG}"
fi

printf 'summary=%s\n' "${SUMMARY_FILE#"${ROOT_DIR}"/}"
exit "${status}"
