#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_nu4_4_3_2_wa_agent_streaming"
CORRELATION_ID="ft-nu4.4.3.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}.jsonl"
SUMMARY_FILE="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_summary.json"
REMOTE_SCRATCH_BASENAME="target-rch-ft-nu4-4-3-2-${RUN_ID}"
# Keep cargo artifacts on the worker's synced workspace volume, but use an
# existing absolute temp root so test processes do not reinterpret TMPDIR
# relative to their own package directory and so we do not need a separate
# non-cargo `rch exec` pre-step.
REMOTE_WORKSPACE_ROOT="${RCH_REMOTE_WORKSPACE_ROOT:-/data/projects/$(basename "${ROOT_DIR}")}"
REMOTE_SCRATCH_ROOT="${RCH_REMOTE_SCRATCH_ROOT:-${REMOTE_WORKSPACE_ROOT}/target/${REMOTE_SCRATCH_BASENAME}}"
REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-/var/tmp}"
REMOTE_TARGET_DIR="${RCH_REMOTE_TARGET_DIR:-${REMOTE_SCRATCH_ROOT}/cargo-target}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|running locally'
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
BENCH_RCH_STEP_TIMEOUT_SECS="${BENCH_RCH_STEP_TIMEOUT_SECS:-1800}"
TIMEOUT_BIN=""
COMPOSE_STEP_TIMEOUT_SECS="${COMPOSE_STEP_TIMEOUT_SECS:-300}"
RCH_TRANSIENT_RETRY_MAX="${RCH_TRANSIENT_RETRY_MAX:-1}"

# `cargo bench` uses RCH's build timeout bucket. Cold workers need longer than
# the default 300s for the wa-agent benchmark crate graph, and the fail-open
# fallback is forbidden for this bead.
export RCH_BUILD_TIMEOUT_SEC="${RCH_BUILD_TIMEOUT_SEC:-1200}"

# This harness already performs explicit `rch check`, worker probe/status, and
# fail-closed guarded remote test/bench steps below. Skip the shared smoke
# preflight here because `rch exec -- cargo check --help` is hanging before the
# harness reaches its real validation path.
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "nu4_4_3_2"
ensure_rch_ready

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "distributed.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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

fail_now() {
  local scenario="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input_summary="$6"

  emit_log \
    "failed" \
    "${scenario}" \
    "${decision_path}" \
    "${reason_code}" \
    "${error_code}" \
    "${artifact_path}" \
    "${input_summary}"

  jq -cn \
    --arg run_id "${RUN_ID}" \
    --arg outcome "failed" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact "${artifact_path}" \
    '{
      run_id: $run_id,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact: $artifact
    }' > "${SUMMARY_FILE}"
  exit 1
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

  "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" "$@"
}

is_transient_remote_cargo_failure() {
  local output_log="$1"

  grep -Eq 'could not parse/generate dep info at: .*cargo-target/.+\.d' "${output_log}" &&
    grep -Eq 'No such file or directory \(os error 2\)' "${output_log}"
}

retry_remote_target_dir() {
  local base_target_dir="$1"
  local scenario="$2"
  local attempt="$3"
  local scenario_slug

  scenario_slug="$(printf '%s' "${scenario}" | tr -c '[:alnum:]' '-')"
  printf '%s-%s-retry%s\n' "${base_target_dir}" "${scenario_slug}" "${attempt}"
}

run_logged_command() {
  local output_log="$1"
  shift
  local runner_pid=""
  local monitor_pid=""
  local cmd_status=0

  : > "${output_log}"

  set +e
  (
    cd "${ROOT_DIR}"
    "$@"
  ) > "${output_log}" 2>&1 &
  runner_pid="$!"
  monitor_pid="$(start_rch_fallback_monitor "${runner_pid}" "${output_log}")"

  wait "${runner_pid}"
  cmd_status=$?
  set -e
  stop_rch_fallback_monitor "${monitor_pid}"
  cat "${output_log}"

  return "${cmd_status}"
}

run_rch_guarded() {
  local scenario="$1"
  local decision_path="$2"
  local success_reason="$3"
  local failure_reason="$4"
  local failure_code="$5"
  local output_log="$6"
  local queue_log=""
  local base_remote_target_dir="${REMOTE_TARGET_DIR}"
  local attempt=0
  local active_output_log=""
  shift 6

  while :; do
    local cmd_status=0
    active_output_log="${output_log}"
    if [[ ${attempt} -gt 0 ]]; then
      active_output_log="${output_log%.log}.retry${attempt}.log"
    fi

    set +e
    if declare -F "$1" >/dev/null 2>&1; then
      local shell_fn="$1"
      shift
      export -f "${shell_fn}"
      export REMOTE_TMPDIR REMOTE_TARGET_DIR
      local shell_cmd
      printf -v shell_cmd '%q ' "${shell_fn}" "$@"
      run_logged_command \
        "${active_output_log}" \
        run_with_timeout "${RCH_STEP_TIMEOUT_SECS}" bash -lc "${shell_cmd}"
      cmd_status=$?
      set -- "${shell_fn}" "$@"
    else
      run_logged_command \
        "${active_output_log}" \
        run_with_timeout "${RCH_STEP_TIMEOUT_SECS}" "$@"
      cmd_status=$?
    fi
    set -e

    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${active_output_log}"; then
      fail_now \
        "${scenario}" \
        "${decision_path}.offload_guard" \
        "rch_local_fallback" \
        "remote_offload_required" \
        "$(basename "${active_output_log}")" \
        "rch fell back to local execution; refusing local CPU-intensive run"
    fi

    if [[ ${cmd_status} -eq 124 || ${cmd_status} -eq 137 ]]; then
      queue_log="${active_output_log%.log}.rch_queue_timeout.log"
      if ! rch queue > "${queue_log}" 2>&1; then
        queue_log="${active_output_log}"
      fi
      fail_now \
        "${scenario}" \
        "${decision_path}.stall_guard" \
        "rch_remote_step_timeout" \
        "RCH-REMOTE-STALL" \
        "$(basename "${queue_log}")" \
        "rch command exceeded ${RCH_STEP_TIMEOUT_SECS}s without producing a final result"
    fi

    if [[ ${cmd_status} -eq 0 ]]; then
      if [[ "${active_output_log}" != "${output_log}" ]]; then
        cp "${active_output_log}" "${output_log}"
      fi
      emit_log \
        "passed" \
        "${scenario}" \
        "${decision_path}" \
        "${success_reason}" \
        "none" \
        "$(basename "${output_log}")" \
        "rch remote execution succeeded after ${attempt} transient retries"
      return 0
    fi

    if [[ ${attempt} -lt ${RCH_TRANSIENT_RETRY_MAX} ]] && is_transient_remote_cargo_failure "${active_output_log}"; then
      attempt=$((attempt + 1))
      REMOTE_TARGET_DIR="$(retry_remote_target_dir "${base_remote_target_dir}" "${scenario}" "${attempt}")"
      emit_log \
        "started" \
        "${scenario}" \
        "${decision_path}.retry" \
        "rch_transient_retry_requested" \
        "none" \
        "$(basename "${active_output_log}")" \
        "retry ${attempt}/${RCH_TRANSIENT_RETRY_MAX}; switching remote_target_dir=${REMOTE_TARGET_DIR}"
      continue
    fi

    fail_now \
      "${scenario}" \
      "${decision_path}" \
      "${failure_reason}" \
      "${failure_code}" \
      "$(basename "${active_output_log}")" \
      "rch command failed"
  done
}

rch_remote_exec() {
  env TMPDIR=/tmp \
    rch exec -- \
    env TMPDIR="${REMOTE_TMPDIR}" CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    "$@"
}

rch_remote_build_ft_linux_binary() {
  local workspace_relative_output="$1"
  local remote_workspace_q=""
  local remote_output_q=""
  local remote_target_q=""
  local remote_tmpdir_q=""
  local remote_cmd=""

  printf -v remote_workspace_q '%q' "${REMOTE_WORKSPACE_ROOT}"
  printf -v remote_output_q '%q' "${workspace_relative_output}"
  printf -v remote_target_q '%q' "${REMOTE_TARGET_DIR}"
  printf -v remote_tmpdir_q '%q' "${REMOTE_TMPDIR}"
  remote_cmd="set -euo pipefail; "
  remote_cmd+="cd ${remote_workspace_q}; "
  remote_cmd+="export TMPDIR=${remote_tmpdir_q}; "
  remote_cmd+="export CARGO_TARGET_DIR=${remote_target_q}; "
  remote_cmd+="cargo build -p frankenterm --bin ft --features distributed; "
  remote_cmd+="mkdir -p \$(dirname ${remote_output_q}); "
  remote_cmd+="cp ${remote_target_q}/debug/ft ${remote_output_q}; "
  remote_cmd+="chmod +x ${remote_output_q}"

  env TMPDIR=/tmp \
    rch exec -- \
    bash -lc "${remote_cmd}"
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-nu4.4.3.2 distributed wa-agent e2e+benchmark validation"

emit_log \
  "started" \
  "suite_init" \
  "remote_scratch_config" \
  "remote_scratch_paths_selected" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "remote_scratch_root=${REMOTE_SCRATCH_ROOT}; remote_tmpdir=${REMOTE_TMPDIR}; remote_target_dir=${REMOTE_TARGET_DIR}"

if ! command -v jq >/dev/null 2>&1; then
  fail_now \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for structured logging"
fi

if ! command -v rch >/dev/null 2>&1; then
  fail_now \
    "suite_init" \
    "preflight_rch" \
    "rch_missing" \
    "rch_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rch is required; cargo must not run locally for this bead"
fi

resolve_timeout_bin
if [[ -z "${TIMEOUT_BIN}" ]]; then
  fail_now \
    "suite_init" \
    "preflight_timeout_tool" \
    "timeout_tool_missing" \
    "timeout_not_found" \
    "$(basename "${LOG_FILE}")" \
    "timeout or gtimeout is required to fail closed on stalled remote execution"
fi

emit_log \
  "started" \
  "suite_init" \
  "stall_guard_config" \
  "timeout_guard_enabled" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "rch_step_timeout_secs=${RCH_STEP_TIMEOUT_SECS}; timeout_bin=${TIMEOUT_BIN}"

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

RCH_PROBE_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_rch_workers_probe.json"
RCH_STATUS_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_rch_status.json"
RCH_CHECK_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_rch_check.log"

if rch check > "${RCH_CHECK_LOG}" 2>&1; then
  emit_log \
    "passed" \
    "suite_init" \
    "preflight_rch_check" \
    "rch_ready" \
    "none" \
    "$(basename "${RCH_CHECK_LOG}")" \
    "rch check passed before workers probe/status"
else
  emit_log \
    "started" \
    "suite_init" \
    "preflight_rch_check" \
    "rch_check_degraded_fallback" \
    "none" \
    "$(basename "${RCH_CHECK_LOG}")" \
    "rch check reported degraded status; continuing to workers probe/status capacity gate"
fi

PROBE_REACHABLE="false"
if rch workers probe --all --json > "${RCH_PROBE_LOG}" 2>"${RCH_PROBE_LOG}.stderr"; then
  if probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
    PROBE_REACHABLE="true"
  fi
fi

if [[ "${PROBE_REACHABLE}" != "true" ]]; then
  if ! rch --json status --workers --jobs > "${RCH_STATUS_LOG}" 2>"${RCH_STATUS_LOG}.stderr"; then
    fail_now \
      "suite_init" \
      "preflight_rch_status_command" \
      "rch_status_unavailable" \
      "rch_status_command_failed" \
      "$(basename "${RCH_STATUS_LOG}.stderr")" \
      "rch status command failed after workers probe showed no reachable workers"
  fi

  if ! status_has_remote_capacity "${RCH_STATUS_LOG}"; then
    fail_now \
      "suite_init" \
      "preflight_rch_workers" \
      "rch_workers_unreachable" \
      "remote_worker_unavailable" \
      "$(basename "${RCH_STATUS_LOG}")" \
      "No remote worker capacity from workers probe or rch status; aborting before cargo invocation"
  fi

  emit_log \
    "passed" \
    "suite_init" \
    "preflight_rch_workers_fallback" \
    "rch_probe_unreachable_but_status_healthy" \
    "none" \
    "$(basename "${RCH_STATUS_LOG}")" \
    "workers probe reported no reachable workers, but rch status reports healthy remote capacity"
fi

CORE_E2E_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_core_distributed_streaming_e2e.log"
run_rch_guarded \
  "core_streaming_e2e" \
  "cargo_test_distributed_streaming_e2e" \
  "distributed_streaming_e2e_passed" \
  "distributed_streaming_e2e_failed" \
  "cargo_test_failed" \
  "${CORE_E2E_LOG}" \
  rch_remote_exec \
  cargo test -p frankenterm-core --features distributed --test distributed_streaming_e2e -- --nocapture

LISTENER_E2E_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_listener_stream_path.log"
run_rch_guarded \
  "listener_stream_e2e" \
  "cargo_test_distributed_listener_stream_path" \
  "distributed_listener_stream_path_passed" \
  "distributed_listener_stream_path_failed" \
  "cargo_test_failed" \
  "${LISTENER_E2E_LOG}" \
  rch_remote_exec \
  cargo test -p frankenterm --features distributed distributed_listener_persists_agent_stream_and_surfaces_remote_status_and_query -- --nocapture

BENCH_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_wa_agent_streaming_bench.log"
RCH_STEP_TIMEOUT_SECS="${BENCH_RCH_STEP_TIMEOUT_SECS}" run_rch_guarded \
  "benchmark_smoke" \
  "cargo_bench_wa_agent_streaming_quick" \
  "wa_agent_streaming_bench_passed" \
  "wa_agent_streaming_bench_failed" \
  "cargo_bench_failed" \
  "${BENCH_LOG}" \
  rch_remote_exec \
  cargo bench -p frankenterm-core --features distributed,asupersync-runtime --bench wa_agent_streaming -- --quick

COMPOSE_BINARY_REL="artifacts/e2e/ft_nu4_4_3_2/${RUN_ID}/ft-linux-amd64"
COMPOSE_BINARY_PATH="${ROOT_DIR}/${COMPOSE_BINARY_REL}"
COMPOSE_BUILD_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_compose_binary_build.log"
run_rch_guarded \
  "compose_binary_build" \
  "cargo_build_ft_linux_binary" \
  "distributed_compose_binary_built" \
  "distributed_compose_binary_build_failed" \
  "cargo_build_failed" \
  "${COMPOSE_BUILD_LOG}" \
  rch_remote_build_ft_linux_binary \
  "${COMPOSE_BINARY_REL}"

if [[ ! -x "${COMPOSE_BINARY_PATH}" ]]; then
  fail_now \
    "compose_binary_build" \
    "cargo_build_ft_linux_binary.artifact_sync" \
    "compose_binary_missing" \
    "compose_binary_not_found" \
    "$(basename "${COMPOSE_BUILD_LOG}")" \
    "rch reported success but synchronized ft binary is missing at ${COMPOSE_BINARY_PATH}"
fi

COMPOSE_ARTIFACT_DIR="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_compose_smoke"
COMPOSE_SMOKE_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_compose_smoke.log"
set +e
(
  cd "${ROOT_DIR}"
  run_with_timeout "${COMPOSE_STEP_TIMEOUT_SECS}" \
    bash tests/e2e/distributed_compose_smoke.sh \
    "${COMPOSE_BINARY_PATH}" \
    "${COMPOSE_ARTIFACT_DIR}" \
    "${RUN_ID}"
) 2>&1 | tee "${COMPOSE_SMOKE_LOG}"
compose_status=${PIPESTATUS[0]}
set -e

if [[ ${compose_status} -ne 0 ]]; then
  fail_now \
    "compose_smoke" \
    "docker_compose_distributed_topology" \
    "distributed_compose_smoke_failed" \
    "compose_smoke_failed" \
    "$(basename "${COMPOSE_SMOKE_LOG}")" \
    "compose topology failed; see compose log and exported artifacts directory"
fi

emit_log \
  "passed" \
  "compose_smoke" \
  "docker_compose_distributed_topology" \
  "distributed_compose_smoke_passed" \
  "none" \
  "$(basename "${COMPOSE_SMOKE_LOG}")" \
  "docker compose validated fake-agent, aggregator listener, security rejection, and query visibility"

emit_log \
  "passed" \
  "suite_complete" \
  "summary" \
  "all_checks_passed" \
  "none" \
  "$(basename "${SUMMARY_FILE}")" \
  "distributed stream, listener integration, and benchmark smoke all passed via rch"

jq -cn \
  --arg run_id "${RUN_ID}" \
  --arg outcome "passed" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg remote_scratch_root "${REMOTE_SCRATCH_ROOT}" \
  --arg remote_tmpdir "${REMOTE_TMPDIR}" \
  --arg remote_target_dir "${REMOTE_TARGET_DIR}" \
  --arg core_log "$(basename "${CORE_E2E_LOG}")" \
  --arg listener_log "$(basename "${LISTENER_E2E_LOG}")" \
  --arg bench_log "$(basename "${BENCH_LOG}")" \
  --arg compose_build_log "$(basename "${COMPOSE_BUILD_LOG}")" \
  --arg compose_smoke_log "$(basename "${COMPOSE_SMOKE_LOG}")" \
  --arg compose_artifacts "${COMPOSE_ARTIFACT_DIR}" \
  --arg rch_check "$(basename "${RCH_CHECK_LOG}")" \
  --arg rch_probe "$(basename "${RCH_PROBE_LOG}")" \
  --arg rch_status "$(basename "${RCH_STATUS_LOG}")" \
  '{
    run_id: $run_id,
    outcome: $outcome,
    correlation_id: $correlation_id,
    remote_paths: {
      scratch_root: $remote_scratch_root,
      tmpdir: $remote_tmpdir,
      target_dir: $remote_target_dir
    },
    artifacts: {
      rch_check: $rch_check,
      rch_probe: $rch_probe,
      rch_status: $rch_status,
      core_streaming_e2e_log: $core_log,
      listener_stream_e2e_log: $listener_log,
      wa_agent_streaming_bench_log: $bench_log,
      compose_binary_build_log: $compose_build_log,
      compose_smoke_log: $compose_smoke_log,
      compose_artifacts_dir: $compose_artifacts
    }
  }' > "${SUMMARY_FILE}"

echo "[ft-nu4.4.3.2] PASS"
echo "Summary: ${SUMMARY_FILE}"
