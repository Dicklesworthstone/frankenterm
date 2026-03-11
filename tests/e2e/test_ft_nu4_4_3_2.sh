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
TARGET_DIR="target-rch-ft-nu4-4-3-2-${RUN_ID}"
REMOTE_TARGET_DIR="/tmp/${TARGET_DIR}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|running locally'

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

run_rch_guarded() {
  local scenario="$1"
  local decision_path="$2"
  local success_reason="$3"
  local failure_reason="$4"
  local failure_code="$5"
  local output_log="$6"
  shift 6

  set +e
  (
    cd "${ROOT_DIR}"
    "$@"
  ) 2>&1 | tee "${output_log}"
  local cmd_status=${PIPESTATUS[0]}
  set -e

  if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_log}"; then
    fail_now \
      "${scenario}" \
      "${decision_path}.offload_guard" \
      "rch_local_fallback" \
      "remote_offload_required" \
      "$(basename "${output_log}")" \
      "rch fell back to local execution; refusing local CPU-intensive run"
  fi

  if [[ ${cmd_status} -ne 0 ]]; then
    fail_now \
      "${scenario}" \
      "${decision_path}" \
      "${failure_reason}" \
      "${failure_code}" \
      "$(basename "${output_log}")" \
      "rch command failed"
  fi

  emit_log \
    "passed" \
    "${scenario}" \
    "${decision_path}" \
    "${success_reason}" \
    "none" \
    "$(basename "${output_log}")" \
    "rch remote execution succeeded"
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-nu4.4.3.2 distributed wa-agent e2e+benchmark validation"

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
  env TMPDIR=/tmp \
    rch exec -- \
    env TMPDIR=/tmp CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    cargo test -p frankenterm-core --features distributed --test distributed_streaming_e2e -- --nocapture

LISTENER_E2E_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_listener_stream_path.log"
run_rch_guarded \
  "listener_stream_e2e" \
  "cargo_test_distributed_listener_stream_path" \
  "distributed_listener_stream_path_passed" \
  "distributed_listener_stream_path_failed" \
  "cargo_test_failed" \
  "${LISTENER_E2E_LOG}" \
  env TMPDIR=/tmp \
    rch exec -- \
    env TMPDIR=/tmp CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    cargo test -p frankenterm --features distributed distributed_listener_persists_agent_stream_and_surfaces_remote_status_and_query -- --nocapture

BENCH_LOG="${LOG_DIR}/ft_nu4_4_3_2_${RUN_ID}_wa_agent_streaming_bench.log"
run_rch_guarded \
  "benchmark_smoke" \
  "cargo_bench_wa_agent_streaming_quick" \
  "wa_agent_streaming_bench_passed" \
  "wa_agent_streaming_bench_failed" \
  "cargo_bench_failed" \
  "${BENCH_LOG}" \
  env TMPDIR=/tmp \
    rch exec -- \
    env TMPDIR=/tmp CARGO_TARGET_DIR="${REMOTE_TARGET_DIR}" \
    cargo bench -p frankenterm-core --features distributed,asupersync-runtime --bench wa_agent_streaming -- --quick

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
  --arg core_log "$(basename "${CORE_E2E_LOG}")" \
  --arg listener_log "$(basename "${LISTENER_E2E_LOG}")" \
  --arg bench_log "$(basename "${BENCH_LOG}")" \
  --arg rch_probe "$(basename "${RCH_PROBE_LOG}")" \
  --arg rch_status "$(basename "${RCH_STATUS_LOG}")" \
  '{
    run_id: $run_id,
    outcome: $outcome,
    correlation_id: $correlation_id,
    artifacts: {
      rch_probe: $rch_probe,
      rch_status: $rch_status,
      core_streaming_e2e_log: $core_log,
      listener_stream_e2e_log: $listener_log,
      wa_agent_streaming_bench_log: $bench_log
    }
  }' > "${SUMMARY_FILE}"

echo "[ft-nu4.4.3.2] PASS"
echo "Summary: ${SUMMARY_FILE}"
