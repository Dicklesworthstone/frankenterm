#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_2_2_cx_outcome_contract"
CORRELATION_ID="ft-e34d9.10.2.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_cx_outcome_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/asupersync_cx_outcome_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/asupersync_cx_outcome_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/asupersync_cx_outcome_${RUN_ID}.report.fail.json"
REPORT_RECOVERY="${LOG_DIR}/asupersync_cx_outcome_${RUN_ID}.report.recovery.json"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  local input="$6"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "asupersync_cx_outcome.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input "${input}" \
    --arg decision_outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input: $input,
      decision_outcome: $decision_outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' | tee -a "${LOG_FILE}" >/dev/null
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for cx/outcome contract validation scenario" >&2
  exit 1
fi

VALIDATOR="${ROOT_DIR}/scripts/validate_asupersync_cx_outcome_contract.sh"
if [[ ! -x "${VALIDATOR}" ]]; then
  echo "validator script missing or not executable: ${VALIDATOR}" >&2
  exit 1
fi

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "starting Cx/Outcome contract validation and deterministic failure/recovery path"

TMP_BAD_WORKFLOWS="$(mktemp)"
cleanup() {
  rm -f "${TMP_BAD_WORKFLOWS}"
}
trap cleanup EXIT

emit_log \
  "running" \
  "validator_pass_path" \
  "cx_outcome_validation_start" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "running validator self-test and baseline validation"

set +e
bash "${VALIDATOR}" \
  --self-test \
  --output "${REPORT_OK}" \
  2>&1 | tee -a "${STDOUT_FILE}"
validator_rc=${PIPESTATUS[0]}
set -e

if [[ ${validator_rc} -ne 0 ]]; then
  emit_log \
    "failed" \
    "validator_pass_path" \
    "validator_failed" \
    "cx_outcome_validation_failed" \
    "$(basename "${REPORT_OK}")" \
    "baseline validator run failed unexpectedly"
  exit "${validator_rc}"
fi

if ! jq -e '.status == "passed"' "${REPORT_OK}" >/dev/null; then
  emit_log \
    "failed" \
    "validator_pass_path" \
    "unexpected_status" \
    "expected_pass_status_missing" \
    "$(basename "${REPORT_OK}")" \
    "validator report did not return passed status"
  exit 1
fi

if ! jq -e '.checks | length >= 4' "${REPORT_OK}" >/dev/null; then
  emit_log \
    "failed" \
    "validator_pass_path" \
    "check_count_too_low" \
    "cx_outcome_checks_missing" \
    "$(basename "${REPORT_OK}")" \
    "expected at least 4 validation checks"
  exit 1
fi

emit_log \
  "running" \
  "failure_injection_path" \
  "inject_missing_adapter_function" \
  "none" \
  "$(basename "${TMP_BAD_WORKFLOWS}")" \
  "renaming wait_failure_to_abort_reason function token to verify deterministic failure detection"

python3 - "${ROOT_DIR}/crates/frankenterm-core/src/workflows.rs" "${TMP_BAD_WORKFLOWS}" <<'PY'
from __future__ import annotations

import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text(encoding="utf-8")
mutated = source.replace("fn wait_failure_to_abort_reason(", "fn wait_failure_to_abort_reason_DISABLED(", 1)
Path(sys.argv[2]).write_text(mutated, encoding="utf-8")
PY

set +e
bash "${VALIDATOR}" \
  --workflows-path "${TMP_BAD_WORKFLOWS}" \
  --output "${REPORT_FAIL}" \
  2>&1 | tee -a "${STDOUT_FILE}"
failure_rc=${PIPESTATUS[0]}
set -e

if [[ ${failure_rc} -eq 0 ]]; then
  emit_log \
    "failed" \
    "failure_injection_path" \
    "validator_unexpected_success" \
    "failure_injection_not_detected" \
    "$(basename "${REPORT_FAIL}")" \
    "validator unexpectedly passed failure-injected workflows.rs"
  exit 1
fi

if ! jq -e '.status == "failed" and .error_code == "missing_required_token"' "${REPORT_FAIL}" >/dev/null; then
  emit_log \
    "failed" \
    "failure_injection_path" \
    "unexpected_failure_code" \
    "missing_expected_error_code" \
    "$(basename "${REPORT_FAIL}")" \
    "failure injection did not produce missing_required_token"
  exit 1
fi

emit_log \
  "running" \
  "recovery_path" \
  "revalidate_clean_source" \
  "none" \
  "$(basename "${REPORT_RECOVERY}")" \
  "re-running validator on clean source to verify recovery after failure injection"

set +e
bash "${VALIDATOR}" \
  --output "${REPORT_RECOVERY}" \
  2>&1 | tee -a "${STDOUT_FILE}"
recovery_rc=${PIPESTATUS[0]}
set -e

if [[ ${recovery_rc} -ne 0 ]] || ! jq -e '.status == "passed"' "${REPORT_RECOVERY}" >/dev/null; then
  emit_log \
    "failed" \
    "recovery_path" \
    "recovery_validation_failed" \
    "recovery_path_not_proven" \
    "$(basename "${REPORT_RECOVERY}")" \
    "clean-source revalidation failed after failure injection"
  exit 1
fi

emit_log \
  "passed" \
  "validator_pass_path->failure_injection_path->recovery_path" \
  "cx_outcome_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Cx/Outcome contract validation, deterministic failure injection, and recovery checks passed"

echo "Asupersync Cx/Outcome contract e2e passed. Logs: ${LOG_FILE_REL}"
