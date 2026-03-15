#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# ft-3681t.5.3 E2E: connector sandbox-zone and capability-envelope validation
#
# Validates:
# 1. Sandbox-zone capability envelope enforcement for connector operations.
# 2. Fail-closed behavior on sandbox violations (policy-class failure path).
# 3. Auditable sandbox decision history + operation-envelope metadata.
# 4. Structured evidence emission with rch-offloaded cargo execution.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

SKIP_LIB_TESTS="${FT_SKIP_LIB_TESTS:-0}"
CARGO_TARGET_DIR_OVERRIDE="${FT_CARGO_TARGET_DIR:-target-rch-ft3681t53-e2e}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_5_3_connector_sandbox_zones"
CORRELATION_ID="ft-3681t.5.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_3681t_5_3_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_3681t_5_3_${RUN_ID}.stdout.log"
STDERR_FILE="${LOG_DIR}/ft_3681t_5_3_${RUN_ID}.stderr.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "3681t_5_3_connector_sandbox_zones"
ensure_rch_ready

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
    --arg component "connector_host_runtime.sandbox.e2e" \
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

emit_log \
  "started" \
  "suite_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-3681t.5.3 sandbox zone + capability envelope validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for structured e2e logs"
  exit 1
fi

if [[ "${SKIP_LIB_TESTS}" != "1" ]]; then
  emit_log \
    "running" \
    "rch_exec_cargo_test_lib" \
    "none" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "rch exec -- env CARGO_TARGET_DIR=${CARGO_TARGET_DIR_OVERRIDE} cargo test -p frankenterm-core --lib connector_host_runtime_sandbox_ -- --nocapture"

  set +e
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR_OVERRIDE}" cargo test -p frankenterm-core --lib connector_host_runtime_sandbox_ -- --nocapture \
    >"${STDOUT_FILE}" 2>"${STDERR_FILE}"
  rc=$?
  set -e

  if [[ ${rc} -ne 0 ]]; then
    emit_log \
      "failed" \
      "rch_exec_cargo_test_lib" \
      "cargo_test_failed" \
      "non_zero_exit" \
      "$(basename "${STDERR_FILE}")" \
      "rch-offloaded connector sandbox unit tests failed with exit ${rc}"
    echo "ft-3681t.5.3 validation failed; inspect ${STDERR_FILE}" >&2
    exit "${rc}"
  fi
else
  : >"${STDOUT_FILE}"
  : >"${STDERR_FILE}"
  emit_log \
    "skipped" \
    "rch_exec_cargo_test_lib" \
    "lib_tests_skipped_by_env" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "FT_SKIP_LIB_TESTS=1; skipping lib test phase by explicit operator choice"
fi

emit_log \
  "running" \
  "rch_exec_cargo_test_integration" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "rch exec -- env CARGO_TARGET_DIR=${CARGO_TARGET_DIR_OVERRIDE} cargo test -p frankenterm-core --test connector_host_runtime_integration connector_host_runtime_integration_sandbox_ -- --nocapture"

set +e
rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR_OVERRIDE}" cargo test -p frankenterm-core --test connector_host_runtime_integration connector_host_runtime_integration_sandbox_ -- --nocapture \
  >>"${STDOUT_FILE}" 2>>"${STDERR_FILE}"
rc=$?
set -e

if [[ ${rc} -ne 0 ]]; then
  emit_log \
    "failed" \
    "rch_exec_cargo_test_integration" \
    "cargo_test_failed" \
    "non_zero_exit" \
    "$(basename "${STDERR_FILE}")" \
    "rch-offloaded connector sandbox integration tests failed with exit ${rc}"
  echo "ft-3681t.5.3 validation failed; inspect ${STDERR_FILE}" >&2
  exit "${rc}"
fi

if ! rg -q "test result: ok" "${STDOUT_FILE}" "${STDERR_FILE}"; then
  emit_log \
    "failed" \
    "validate_test_signature" \
    "missing_success_signature" \
    "unexpected_test_output" \
    "$(basename "${STDOUT_FILE}")" \
    "cargo test output missing 'test result: ok' signature"
  echo "ft-3681t.5.3 validation failed; test success signature missing" >&2
  exit 1
fi

emit_log \
  "passed" \
  "suite_complete" \
  "all_checks_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "connector sandbox-zone and capability-envelope checks passed"

echo "ft-3681t.5.3 e2e validation passed. Log: ${LOG_FILE}"
