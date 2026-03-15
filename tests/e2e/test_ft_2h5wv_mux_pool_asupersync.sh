#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_2h5wv_mux_pool_asupersync"
CORRELATION_ID="ft-2h5wv-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mux_pool_asupersync_${RUN_ID}.jsonl"
SUMMARY_FILE="${LOG_DIR}/mux_pool_asupersync_${RUN_ID}_summary.json"

REMOTE_SCRATCH_BASENAME="target-rch-ft-2h5wv-${RUN_ID}"
REMOTE_SCRATCH_ROOT="${RCH_REMOTE_SCRATCH_ROOT:-target/${REMOTE_SCRATCH_BASENAME}}"
REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-${REMOTE_SCRATCH_ROOT}/tmp}"
REMOTE_TARGET_DIR="${RCH_REMOTE_TARGET_DIR:-${REMOTE_SCRATCH_ROOT}/cargo-target}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""

PASS=0
FAIL=0
RCH_RUNTIME_BLOCKED="false"
RCH_RUNTIME_BLOCK_REASON=""

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "mux_pool_asupersync.e2e" \
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

write_summary() {
  local outcome="passed"
  if [ "${FAIL}" -gt 0 ]; then
    outcome="failed"
  fi

  jq -cn \
    --arg run_id "${RUN_ID}" \
    --arg outcome "${outcome}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg remote_scratch_root "${REMOTE_SCRATCH_ROOT}" \
    --arg remote_tmpdir "${REMOTE_TMPDIR}" \
    --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
    --argjson pass "${PASS}" \
    --argjson fail "${FAIL}" \
    --argjson total "$((PASS + FAIL))" \
    '{
      run_id: $run_id,
      outcome: $outcome,
      correlation_id: $correlation_id,
      pass: $pass,
      fail: $fail,
      total: $total,
      remote_scratch_root: $remote_scratch_root,
      remote_tmpdir: $remote_tmpdir,
      remote_target_dir: $remote_target_dir
    }' > "${SUMMARY_FILE}"
}

fatal() {
  local decision_path="$1"
  local reason_code="$2"
  local error_code="$3"
  local artifact_path="$4"
  local input_summary="$5"

  emit_log "failed" "${decision_path}" "${reason_code}" "${error_code}" "${artifact_path}" "${input_summary}"
  FAIL=$((FAIL + 1))
  write_summary
  echo "fatal: ${decision_path} (${reason_code}/${error_code})" >&2
  exit 1
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    fatal "preflight_${cmd}" "missing_prerequisite" "E2E-PREREQ" "${LOG_FILE}" "missing:${cmd}"
  fi
}

resolve_timeout_bin() {
  if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
  elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
  else
    TIMEOUT_BIN=""
  fi
}

run_with_timeout() {
  local timeout_secs="$1"
  shift

  "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" bash -lc "$*"
}

step_timed_out() {
  local rc="$1"
  [[ "${rc}" -eq 124 || "${rc}" -eq 137 ]]
}

probe_has_reachable_workers() {
  local probe_log="$1"
  jq -e '[.data[]? | (.status // "" | ascii_downcase) | select(. == "ok" or . == "healthy" or . == "reachable")] | length > 0' \
    "${probe_log}" >/dev/null
}

status_has_remote_capacity() {
  local status_log="$1"
  jq -e '(.data.daemon.workers_healthy // 0) > 0 and (.data.daemon.slots_total // 0) > 0' \
    "${status_log}" >/dev/null
}

rch_remote_exec() {
  env TMPDIR=/tmp \
    rch exec -- \
    env TMPDIR="${REMOTE_TMPDIR}" CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    "$@"
}

run_rch_guarded() {
  local scenario="$1"
  local decision_path="$2"
  local success_reason="$3"
  local failure_reason="$4"
  local failure_code="$5"
  local output_log="$6"
  local queue_log=""
  local fail_open_flag="${output_log}.fail_open_detected"
  local pid_file="${output_log}.pid"
  shift 6

  rm -f "${fail_open_flag}" "${pid_file}"
  set +e
  (
    cd "${ROOT_DIR}"
    run_with_timeout "${RCH_STEP_TIMEOUT_SECS}" "$@"
  ) > >(
    tee "${output_log}" | while IFS= read -r line; do
      printf '%s\n' "${line}"
      if printf '%s\n' "${line}" | grep -Eq "${RCH_FAIL_OPEN_REGEX}"; then
        : > "${fail_open_flag}"
        if [[ -f "${pid_file}" ]]; then
          kill -TERM "$(cat "${pid_file}")" 2>/dev/null || true
        fi
      fi
    done
  ) 2>&1 &
  local cmd_pid=$!
  printf '%s\n' "${cmd_pid}" > "${pid_file}"
  wait "${cmd_pid}"
  local cmd_status=$?
  set -e
  rm -f "${pid_file}"

  if [[ -f "${fail_open_flag}" ]] || grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_log}"; then
    rm -f "${fail_open_flag}"
    RCH_RUNTIME_BLOCKED="true"
    RCH_RUNTIME_BLOCK_REASON="rch_local_fallback"
    emit_log \
      "failed" \
      "${decision_path}.offload_guard" \
      "rch_local_fallback" \
      "remote_offload_required" \
      "$(basename "${output_log}")" \
      "rch fell back to local execution; refusing local CPU-intensive run"
    return 1
  fi
  rm -f "${fail_open_flag}"

  if step_timed_out "${cmd_status}"; then
    queue_log="${output_log%.log}.rch_queue_timeout.log"
    if ! rch queue > "${queue_log}" 2>&1; then
      queue_log="${output_log}"
    fi
    RCH_RUNTIME_BLOCKED="true"
    RCH_RUNTIME_BLOCK_REASON="rch_remote_step_timeout"
    emit_log \
      "failed" \
      "${decision_path}.stall_guard" \
      "rch_remote_step_timeout" \
      "RCH-REMOTE-STALL" \
      "$(basename "${queue_log}")" \
      "rch command exceeded ${RCH_STEP_TIMEOUT_SECS}s without producing a final result"
    return 1
  fi

  if [[ "${cmd_status}" -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "${failure_reason}" \
      "${failure_code}" \
      "$(basename "${output_log}")" \
      "rch command failed"
    return 1
  fi

  emit_log \
    "passed" \
    "${decision_path}" \
    "${success_reason}" \
    "none" \
    "$(basename "${output_log}")" \
    "rch remote execution succeeded"
  return 0
}

echo "=== MuxPool asupersync migration validation (ft-2h5wv) ==="
echo "Run ID:     ${RUN_ID}"
echo "Log:        ${LOG_FILE#"${ROOT_DIR}"/}"
echo ""

require_cmd jq
require_cmd rch

resolve_timeout_bin
if [[ -z "${TIMEOUT_BIN}" ]]; then
  fatal "preflight_timeout_tool" "timeout_tool_missing" "timeout_not_found" "$(basename "${LOG_FILE}")" \
    "timeout or gtimeout is required to fail closed on stalled remote execution"
fi

export REMOTE_TMPDIR
export REMOTE_TARGET_DIR
export -f rch_remote_exec

emit_log "started" "stall_guard_config" "timeout_guard_enabled" "none" "$(basename "${LOG_FILE}")" \
  "rch_step_timeout_secs=${RCH_STEP_TIMEOUT_SECS}; timeout_bin=${TIMEOUT_BIN}"

RCH_CHECK_LOG="${LOG_DIR}/ft_2h5wv_${RUN_ID}_rch_check.log"
RCH_PROBE_LOG="${LOG_DIR}/ft_2h5wv_${RUN_ID}_rch_workers_probe.json"
RCH_STATUS_LOG="${LOG_DIR}/ft_2h5wv_${RUN_ID}_rch_status.json"

if ! rch check > "${RCH_CHECK_LOG}" 2>&1; then
  fatal "preflight_rch_check" "rch_not_ready" "rch_check_failed" "$(basename "${RCH_CHECK_LOG}")" \
    "rch check failed before workers probe/status"
fi

emit_log "passed" "preflight_rch_check" "rch_ready" "none" "$(basename "${RCH_CHECK_LOG}")" \
  "rch check passed before workers probe/status"

PROBE_REACHABLE="false"
if rch workers probe --all --json > "${RCH_PROBE_LOG}" 2>"${RCH_PROBE_LOG}.stderr"; then
  if probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
    PROBE_REACHABLE="true"
  fi
fi

if [[ "${PROBE_REACHABLE}" != "true" ]]; then
  if ! rch --json status --workers --jobs > "${RCH_STATUS_LOG}" 2>"${RCH_STATUS_LOG}.stderr"; then
    fatal "preflight_rch_status_command" "rch_status_unavailable" "rch_status_command_failed" \
      "$(basename "${RCH_STATUS_LOG}.stderr")" \
      "rch status command failed after workers probe showed no reachable workers"
  fi

  if ! status_has_remote_capacity "${RCH_STATUS_LOG}"; then
    fatal "preflight_rch_workers" "rch_workers_unreachable" "remote_worker_unavailable" \
      "$(basename "${RCH_STATUS_LOG}")" \
      "No remote worker capacity from workers probe or rch status; aborting before cargo invocation"
  fi

  emit_log "passed" "preflight_rch_workers_fallback" "rch_probe_unreachable_but_status_healthy" "none" \
    "$(basename "${RCH_STATUS_LOG}")" \
    "workers probe reported no reachable workers, but rch status reports healthy remote capacity"
fi

echo -n "S1: Cx-threaded API completeness... "
CX_METHODS=$(grep -c 'pub async fn.*_with_cx' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${CX_METHODS}" -ge 7 ]; then
  echo "PASS (${CX_METHODS} Cx methods found)"
  emit_log "pass" "cx_api_completeness" "cx_methods_found" "" "" "cx_methods=${CX_METHODS}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${CX_METHODS} Cx methods, expected >=7)"
  emit_log "fail" "cx_api_completeness" "insufficient_cx_methods" "E_CX_GAP" "" "cx_methods=${CX_METHODS}"
  FAIL=$((FAIL + 1))
fi

echo -n "S2: No tokio::test in mux_pool... "
TOKIO_TESTS=$(grep -c '#\[tokio::test\]' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${TOKIO_TESTS}" -eq 0 ]; then
  echo "PASS"
  emit_log "pass" "no_tokio_test" "clean" "" "" "tokio_tests=0"
  PASS=$((PASS + 1))
else
  echo "FAIL (${TOKIO_TESTS} tokio::test attrs remain)"
  emit_log "fail" "no_tokio_test" "tokio_remnants" "E_TOKIO" "" "tokio_tests=${TOKIO_TESTS}"
  FAIL=$((FAIL + 1))
fi

echo -n "S3: Structured diagnostics coverage... "
DIAG_EVENTS=$(grep -c 'subsystem = "mux_pool"' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${DIAG_EVENTS}" -ge 8 ]; then
  echo "PASS (${DIAG_EVENTS} diagnostic events)"
  emit_log "pass" "diagnostics_coverage" "sufficient" "" "" "diag_events=${DIAG_EVENTS}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${DIAG_EVENTS} diagnostic events, expected >=8)"
  emit_log "fail" "diagnostics_coverage" "insufficient" "E_DIAG" "" "diag_events=${DIAG_EVENTS}"
  FAIL=$((FAIL + 1))
fi

echo -n "S4: Test count >= 50... "
TEST_COUNT=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${TEST_COUNT}" -ge 50 ]; then
  echo "PASS (${TEST_COUNT} tests)"
  emit_log "pass" "test_count" "sufficient" "" "" "test_count=${TEST_COUNT}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${TEST_COUNT} tests, expected >=50)"
  emit_log "fail" "test_count" "insufficient" "E_TESTS" "" "test_count=${TEST_COUNT}"
  FAIL=$((FAIL + 1))
fi

echo -n "S5: Recovery code paths... "
RECOVERY_CX=$(grep -c 'execute_with_recovery_with_cx' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
RECOVERY_INNER=$(grep -c 'execute_with_recovery_inner' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${RECOVERY_CX}" -ge 2 ] && [ "${RECOVERY_INNER}" -ge 2 ]; then
  echo "PASS (cx=${RECOVERY_CX}, inner=${RECOVERY_INNER})"
  emit_log "pass" "recovery_paths" "dual_path" "" "" "cx=${RECOVERY_CX},inner=${RECOVERY_INNER}"
  PASS=$((PASS + 1))
else
  echo "FAIL (cx=${RECOVERY_CX}, inner=${RECOVERY_INNER})"
  emit_log "fail" "recovery_paths" "missing_path" "E_RECOVERY" "" "cx=${RECOVERY_CX},inner=${RECOVERY_INNER}"
  FAIL=$((FAIL + 1))
fi

echo -n "S6: MuxPoolStats serde... "
SERDE_DERIVE=$(grep -c 'Serialize, Deserialize' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${SERDE_DERIVE}" -ge 1 ]; then
  echo "PASS"
  emit_log "pass" "stats_serde" "present" "" "" "serde_derives=${SERDE_DERIVE}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "stats_serde" "missing" "E_SERDE" "" ""
  FAIL=$((FAIL + 1))
fi

echo -n "S7: Pipeline batch fallback... "
FALLBACK=$(grep -c 'falling back to sequential' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${FALLBACK}" -ge 2 ]; then
  echo "PASS (${FALLBACK} fallback paths)"
  emit_log "pass" "pipeline_fallback" "present" "" "" "fallback_paths=${FALLBACK}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "pipeline_fallback" "missing" "E_PIPELINE" "" "fallback_paths=${FALLBACK}"
  FAIL=$((FAIL + 1))
fi

echo -n "S8: Ambient (non-Cx) API surface... "
AMBIENT_ACQUIRE=$(grep -c 'acquire_client_inner' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${AMBIENT_ACQUIRE}" -ge 2 ]; then
  echo "PASS"
  emit_log "pass" "ambient_api" "present" "" "" "ambient_acquire_refs=${AMBIENT_ACQUIRE}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "ambient_api" "missing" "E_AMBIENT" "" ""
  FAIL=$((FAIL + 1))
fi

echo -n "S9: rch-offloaded health-check regression tests... "
HEALTH_CHECK_LOG="${LOG_DIR}/ft_2h5wv_${RUN_ID}_health_check_with_cx.log"
if run_rch_guarded \
  "health_check_with_cx" \
  "cargo_test_health_check_with_cx" \
  "health_check_with_cx_tests_passed" \
  "health_check_with_cx_tests_failed" \
  "cargo_test_failed" \
  "${HEALTH_CHECK_LOG}" \
  rch_remote_exec \
  'cargo test -p frankenterm-core --features vendored,asupersync-runtime pool_health_check_with_cx_ -- --nocapture'; then
  echo "PASS"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  FAIL=$((FAIL + 1))
fi

echo -n "S10: rch-offloaded batch fallback regression tests... "
BATCH_FALLBACK_LOG="${LOG_DIR}/ft_2h5wv_${RUN_ID}_batch_fallback_with_cx.log"
if [[ "${RCH_RUNTIME_BLOCKED}" == "true" ]]; then
  echo "FAIL (skipped after ${RCH_RUNTIME_BLOCK_REASON})"
  emit_log "failed" "cargo_test_batch_fallback_with_cx.preflight_skip" "rch_runtime_blocked" "remote_offload_required" \
    "$(basename "${BATCH_FALLBACK_LOG}")" "prior rch step set runtime block reason=${RCH_RUNTIME_BLOCK_REASON}"
  FAIL=$((FAIL + 1))
else
  if run_rch_guarded \
    "batch_fallback_with_cx" \
    "cargo_test_batch_fallback_with_cx" \
    "batch_fallback_with_cx_tests_passed" \
    "batch_fallback_with_cx_tests_failed" \
    "cargo_test_failed" \
    "${BATCH_FALLBACK_LOG}" \
    rch_remote_exec \
    'cargo test -p frankenterm-core --features vendored,asupersync-runtime pool_batch_render_with_cx_ -- --nocapture'; then
    echo "PASS"
    PASS=$((PASS + 1))
  else
    echo "FAIL"
    FAIL=$((FAIL + 1))
  fi
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log:     ${LOG_FILE#"${ROOT_DIR}"/}"
echo "Summary: ${SUMMARY_FILE#"${ROOT_DIR}"/}"

write_summary

if [ "${FAIL}" -gt 0 ]; then
  exit 1
fi
