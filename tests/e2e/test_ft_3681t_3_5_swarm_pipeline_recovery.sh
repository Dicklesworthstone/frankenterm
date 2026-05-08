#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-3681t.3.5 E2E: swarm pipeline recovery/compensation behavior
#
# Validates:
# 1. Failure injection path drives retry/recovery hooks deterministically.
# 2. Compensation path executes and emits expected metadata contracts.
# 3. Degraded dependency-gate behavior fails closed for required dependents.
# 4. Structured artifacts and logs are emitted for triage.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_3_5_swarm_pipeline_recovery"
CORRELATION_ID="ft-3681t.3.5-${RUN_ID}"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-3681t-3-5-${RUN_ID}"
REQUESTED_CARGO_TARGET_DIR="${FT_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
if [[ -n "${REQUESTED_CARGO_TARGET_DIR}" && "${REQUESTED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${REQUESTED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

LOG_FILE="${LOG_DIR}/ft_3681t_3_5_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_3681t_3_5_${RUN_ID}.stdout.log"
STDERR_FILE="${LOG_DIR}/ft_3681t_3_5_${RUN_ID}.stderr.log"

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
    --arg component "swarm_pipeline.e2e" \
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

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured e2e logging and shared rch metadata" >&2
  exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "3681t_3_5_swarm_pipeline_recovery"
ensure_rch_ready

emit_log \
  "started" \
  "suite_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-3681t.3.5 integration/e2e swarm pipeline recovery scenario"

emit_log \
  "passed" \
  "preflight_rch_workers" \
  "rch_shared_guard_passed" \
  "none" \
  "$(basename "$(rch_probe_log_path)")" \
  "shared rch guard reported reachable workers"

emit_log \
  "passed" \
  "preflight_rch_remote_smoke" \
  "rch_remote_smoke_passed" \
  "none" \
  "$(basename "$(rch_smoke_log_path)")" \
  "shared rch guard verified fail-closed remote cargo execution"

emit_log \
  "running" \
  "rch_exec_cargo_test_integration" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} cargo test -p frankenterm-core --test swarm_pipeline_integration -- --nocapture"

set +e
run_rch_cargo_logged "${STDOUT_FILE}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test swarm_pipeline_integration -- --nocapture
rc=$?
set -e
: >"${STDERR_FILE}"

if [[ ${rc} -ne 0 ]]; then
  emit_log \
    "failed" \
    "rch_exec_cargo_test_integration" \
    "cargo_test_failed" \
    "non_zero_exit" \
    "$(basename "${STDOUT_FILE}")" \
    "integration test execution failed with exit ${rc}"
  echo "ft-3681t.3.5 e2e failed; inspect ${STDOUT_FILE}" >&2
  exit "${rc}"
fi

if ! rg -q "test result: ok" "${STDOUT_FILE}" "${STDERR_FILE}"; then
  emit_log \
    "failed" \
    "validate_test_signature" \
    "missing_success_signature" \
    "unexpected_test_output" \
    "$(basename "${STDOUT_FILE}")" \
    "test output missing 'test result: ok'"
  echo "ft-3681t.3.5 e2e failed; missing success signature" >&2
  exit 1
fi

if ! rg -q '"component":"swarm_pipeline.integration"' "${STDOUT_FILE}" "${STDERR_FILE}"; then
  emit_log \
    "failed" \
    "validate_structured_logs" \
    "missing_structured_log_payload" \
    "integration_log_missing" \
    "$(basename "${STDOUT_FILE}")" \
    "expected structured integration log lines were not emitted"
  echo "ft-3681t.3.5 e2e failed; integration structured logs missing" >&2
  exit 1
fi

emit_log \
  "passed" \
  "suite_complete" \
  "all_checks_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "swarm pipeline recovery/compensation/degraded-path checks passed"

echo "ft-3681t.3.5 e2e validation passed. Log: ${LOG_FILE}"
