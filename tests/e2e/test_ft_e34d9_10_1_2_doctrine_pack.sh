#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_1_2_doctrine_pack"
CORRELATION_ID="ft-e34d9.10.1.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_runtime_doctrine_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/asupersync_runtime_doctrine_${RUN_ID}.stdout.log"
REPORT_OK="${LOG_DIR}/asupersync_runtime_doctrine_${RUN_ID}.report.ok.json"
REPORT_FAIL="${LOG_DIR}/asupersync_runtime_doctrine_${RUN_ID}.report.fail.json"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

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
    --arg component "asupersync_runtime_doctrine.e2e" \
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
    }' | tee -a "${LOG_FILE}" >/dev/null
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for doctrine pack validation scenario" >&2
  exit 1
fi

VALIDATOR="${ROOT_DIR}/scripts/validate_asupersync_doctrine_pack.sh"
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
  "starting doctrine contract validation + failure-injection checks"

TMP_BAD_CONTRACT="$(mktemp)"
cleanup() {
  rm -f "${TMP_BAD_CONTRACT}"
}
trap cleanup EXIT

emit_log \
  "running" \
  "validator_pass_path" \
  "doctrine_validation_start" \
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
    "doctrine_validation_failed" \
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

if ! jq -e '.integration_summary.module_checks_passed >= 3' "${REPORT_OK}" >/dev/null; then
  emit_log \
    "failed" \
    "validator_pass_path" \
    "module_check_count_too_low" \
    "integration_module_checks_missing" \
    "$(basename "${REPORT_OK}")" \
    "expected at least 3 representative module checks"
  exit 1
fi

emit_log \
  "running" \
  "failure_injection_path" \
  "inject_missing_user_guarantee" \
  "none" \
  "$(basename "${TMP_BAD_CONTRACT}")" \
  "removing required user guarantee to verify validator fails deterministically"

python3 - "${ROOT_DIR}/docs/asupersync-runtime-doctrine-v1.json" "${TMP_BAD_CONTRACT}" <<'PY'
from __future__ import annotations

import json
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text(encoding="utf-8")
contract = json.loads(source)
contract["user_facing_guarantees"] = [
    item
    for item in contract.get("user_facing_guarantees", [])
    if item.get("id") != "no_silent_command_event_loss"
]
Path(sys.argv[2]).write_text(json.dumps(contract, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

set +e
bash "${VALIDATOR}" \
  --contract-path "${TMP_BAD_CONTRACT}" \
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
    "validator unexpectedly passed failure-injected contract"
  exit 1
fi

if ! jq -e '.status == "failed" and .error_code == "missing_user_guarantee"' "${REPORT_FAIL}" >/dev/null; then
  emit_log \
    "failed" \
    "failure_injection_path" \
    "unexpected_failure_code" \
    "missing_expected_error_code" \
    "$(basename "${REPORT_FAIL}")" \
    "failure injection did not produce missing_user_guarantee"
  exit 1
fi

emit_log \
  "passed" \
  "validator_pass_path->failure_injection_path" \
  "runtime_doctrine_pack_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "doctrine contract, integration checks, and deterministic failure-injection validated"

echo "Asupersync runtime doctrine pack e2e passed. Logs: ${LOG_FILE_REL}"
