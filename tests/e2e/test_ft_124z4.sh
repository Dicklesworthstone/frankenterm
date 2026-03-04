#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_124z4"
CORRELATION_ID="ft-124z4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft124z4}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|Failed to connect to ubuntu@'

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

rch_fail_open_detected() {
  local log_path="$1"
  grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${log_path}"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rch
require_cmd cargo

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

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
  emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_workers_unreachable" "RCH-E100" "$(basename "${probe_log}")"
  echo "no reachable rch workers; refusing local fallback" >&2
  exit 2
fi

emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"

emit_log "preflight" "rch_remote_smoke" "cargo_version" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step rch_remote_smoke rch exec -- env TMPDIR=/tmp cargo --version; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    emit_log "preflight" "rch_remote_smoke" "cargo_version" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch remote smoke check failed-open to local execution; refusing offload policy violation" >&2
    exit 3
  fi
  emit_log "preflight" "rch_remote_smoke" "cargo_version" "passed" "remote_exec_confirmed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "preflight" "rch_remote_smoke" "cargo_version" "failed" "rch_remote_smoke_failed" "RCH-REMOTE-SMOKE-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 2
fi

emit_log "validation" "nominal_path" "tailer_labruntime_tests" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step nominal_labruntime \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test tailer_labruntime --features asupersync-runtime -- --nocapture; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    emit_log "validation" "nominal_path" "tailer_labruntime_tests" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
  emit_log "validation" "nominal_path" "tailer_labruntime_tests" "passed" "tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "validation" "nominal_path" "tailer_labruntime_tests" "failed" "test_failure" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "validation" "failure_injection_path" "bench_without_feature" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
set +e
run_step failure_missing_feature \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p frankenterm-core --bench tailer --message-format short
missing_feature_rc=$?
set -e

if rch_fail_open_detected "${LAST_STEP_LOG}"; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
  echo "rch fell back to local execution; failing per offload-only policy" >&2
  exit 3
fi

if [[ ${missing_feature_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "expected_failure_missing" "EXPECTED-FAILURE-NOT-TRIGGERED" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

if ! grep -q "requires the features: .*asupersync-runtime" "${LAST_STEP_LOG}"; then
  emit_log "validation" "failure_injection_path" "bench_without_feature" "failed" "unexpected_error_signature" "FEATURE-GATE-SIGNATURE-MISSING" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "validation" "failure_injection_path" "bench_without_feature" "passed" "expected_feature_gate_failure" "none" "$(basename "${LAST_STEP_LOG}")"

emit_log "validation" "recovery_path" "bench_with_feature" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step recovery_with_feature \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo check -p frankenterm-core --bench tailer --features asupersync-runtime --message-format short; then
  if rch_fail_open_detected "${LAST_STEP_LOG}"; then
    emit_log "validation" "recovery_path" "bench_with_feature" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
  emit_log "validation" "recovery_path" "bench_with_feature" "passed" "recovery_success" "none" "$(basename "${LAST_STEP_LOG}")"
else
  emit_log "validation" "recovery_path" "bench_with_feature" "failed" "recovery_failed" "CARGO-CHECK-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "summary" "nominal->failure_injection->recovery" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"

echo "ft-124z4 e2e scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
