#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_6_1"
CORRELATION_ID="ft-e34d9.10.6.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft-e34d9-10-6-1}-${RUN_ID}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""

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

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return ${rc}
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

run_rch_test_step() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "$@"; then
    if grep -q "\[RCH\] local" "${LAST_STEP_LOG}"; then
      emit_log "validation" "${decision_path}" "${input_summary}" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
      echo "rch fell back to local execution; failing per offload-only policy" >&2
      exit 3
    fi
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "test_failure" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rch
require_cmd cargo

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

check_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_check.log"
set +e
rch check > "${check_log}" 2>&1
check_rc=$?
set -e
cat "${check_log}" >> "${STDOUT_FILE}"
if [[ ${check_rc} -ne 0 ]]; then
  emit_log "preflight" "rch_check" "health_check" "failed" "rch_check_failed" "RCH-E100" "$(basename "${check_log}")"
  echo "rch check failed" >&2
  exit 2
fi
emit_log "preflight" "rch_check" "health_check" "passed" "rch_check_ready" "none" "$(basename "${check_log}")"

probe_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_probe.json"
set +e
rch workers probe --all --json > "${probe_log}" 2>>"${STDOUT_FILE}"
probe_rc=$?
set -e

if [[ ${probe_rc} -ne 0 ]]; then
  emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_probe_failed" "RCH-E100" "$(basename "${probe_log}")"
  echo "rch workers probe failed" >&2
  exit 2
fi

healthy_workers=$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${probe_log}")
if [[ "${healthy_workers}" -lt 1 ]]; then
  status_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_status.json"
  set +e
  rch --json status --workers --jobs > "${status_log}" 2>>"${STDOUT_FILE}"
  status_rc=$?
  set -e
  if [[ ${status_rc} -ne 0 ]]; then
    emit_log "preflight" "rch_probe->rch_status" "workers_probe_status_fallback" "failed" "rch_status_failed" "RCH-E100" "$(basename "${status_log}")"
    echo "rch status fallback failed" >&2
    exit 2
  fi

  status_healthy_workers=$(jq '(.data.daemon.workers_healthy // ([.data.workers[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length) // 0)' "${status_log}")
  status_slots_total=$(jq '(.data.daemon.slots_total // ([.data.workers[]? | (.total_slots // 0)] | add) // 0)' "${status_log}")
  if [[ "${status_healthy_workers}" -ge 1 && "${status_slots_total}" -ge 1 ]]; then
    if grep -q "RCH is ready" "${check_log}"; then
      emit_log "preflight" "rch_check->rch_probe->rch_status" "workers_probe_status_fallback" "failed" "rch_health_probe_mismatch" "RCH-E101" "$(basename "${status_log}")"
      echo "rch check/status report healthy but probe shows zero reachable workers; refusing local fallback" >&2
    else
      emit_log "preflight" "rch_probe->rch_status" "workers_probe_status_fallback" "failed" "rch_probe_unreachable_but_status_healthy" "RCH-E100" "$(basename "${status_log}")"
      echo "rch status appears healthy but probe shows zero reachable workers; refusing local fallback" >&2
    fi
  else
    emit_log "preflight" "rch_probe->rch_status" "workers_probe_status_fallback" "failed" "rch_workers_unreachable_probe" "RCH-E100" "$(basename "${status_log}")"
    echo "no reachable rch workers; refusing local fallback" >&2
  fi
  exit 2
else
  emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"
fi

run_rch_test_step \
  "tailer_restart_state_machine" \
  "deterministic.watch_ingest.restart" \
  "test=lab_tailer_sync_handles_pane_restart_without_resurrecting_removed_pane;seed=1337" \
  cargo test -p frankenterm-core --test tailer_labruntime --features asupersync-runtime -- --nocapture lab_tailer_sync_handles_pane_restart_without_resurrecting_removed_pane

run_rch_test_step \
  "distributed_reconnect_state_machine" \
  "deterministic.ipc_handler.reconnect" \
  "test=dpor_distributed_reconnect_replay_preserves_contiguous_sequence;base_seed=89" \
  cargo test -p frankenterm-core --test distributed_merge_dpor --features asupersync-runtime,distributed -- --nocapture dpor_distributed_reconnect_replay_preserves_contiguous_sequence

run_rch_test_step \
  "streaming_subscriber_restart_state_machine" \
  "deterministic.scheduler.restart_suffix" \
  "test=dpor_stream_reconnect_receives_ordered_suffix_after_restart;base_seed=211" \
  cargo test -p frankenterm-core --test web_streaming_dpor --features asupersync-runtime,web -- --nocapture dpor_stream_reconnect_receives_ordered_suffix_after_restart

emit_log "summary" "nominal_suite" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"

echo "ft-e34d9.10.6.1 e2e scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
