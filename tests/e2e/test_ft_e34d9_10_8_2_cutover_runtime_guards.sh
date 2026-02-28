#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_8_2_cutover_runtime_guards"
CORRELATION_ID="ft-e34d9.10.8.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

REPORT_OK="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.report.ok.json"
REPORT_REPEAT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.report.repeat.json"
REPORT_FAIL="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.report.fail.json"
REPORT_RECOVERY="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.report.recovery.json"

BASE_CARGO_TARGET_DIR="target/rch-e2e-ft-e34d9-10-8-2"
if [[ -n "${CARGO_TARGET_DIR:-}" && "${CARGO_TARGET_DIR}" == target/* ]]; then
  BASE_CARGO_TARGET_DIR="${CARGO_TARGET_DIR}"
fi
CARGO_TARGET_DIR="${BASE_CARGO_TARGET_DIR%/}-${RUN_ID}"
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
  return "${rc}"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

ensure_rch_remote_only() {
  if grep -q "\[RCH\] local" "${LAST_STEP_LOG}"; then
    emit_log "validation" "rch_offload_policy" "remote_exec_required" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
}

run_rch_step() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "$@"; then
    ensure_rch_remote_only
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "rch_step_passed" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    ensure_rch_remote_only
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "rch_step_failed" "RCH-STEP-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd python3
require_cmd rch
require_cmd cargo

VALIDATOR="${ROOT_DIR}/scripts/validate_asupersync_cutover_runtime_guards.sh"
POLICY="${ROOT_DIR}/docs/asupersync-cutover-runtime-guardrails.json"

for artifact in "${VALIDATOR}" "${POLICY}"; do
  if [[ ! -f "${artifact}" ]]; then
    emit_log "preflight" "required_artifacts" "artifact=${artifact}" "failed" "missing_artifact" "ARTIFACT-MISSING" "${artifact}"
    echo "required artifact missing: ${artifact}" >&2
    exit 1
  fi
done

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

emit_log "validation" "guard_validator.nominal" "run=baseline+selftest" "running" "none" "none" "$(basename "${REPORT_OK}")"
if bash "${VALIDATOR}" --self-test --policy-path "${POLICY}" --output "${REPORT_OK}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "guard_validator.nominal" "run=baseline+selftest" "passed" "validator_passed" "none" "$(basename "${REPORT_OK}")"
else
  emit_log "validation" "guard_validator.nominal" "run=baseline+selftest" "failed" "validator_failed" "GUARD-VALIDATOR-FAIL" "$(basename "${REPORT_OK}")"
  exit 1
fi

if ! jq -e '.status == "passed"' "${REPORT_OK}" >/dev/null; then
  emit_log "validation" "guard_validator.nominal" "report_status_check" "failed" "report_status_not_passed" "GUARD-REPORT-STATUS" "$(basename "${REPORT_OK}")"
  exit 1
fi

emit_log "validation" "guard_validator.repeat" "run=repeat_no_selftest" "running" "none" "none" "$(basename "${REPORT_REPEAT}")"
if bash "${VALIDATOR}" --policy-path "${POLICY}" --output "${REPORT_REPEAT}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "guard_validator.repeat" "run=repeat_no_selftest" "passed" "repeat_validator_passed" "none" "$(basename "${REPORT_REPEAT}")"
else
  emit_log "validation" "guard_validator.repeat" "run=repeat_no_selftest" "failed" "repeat_validator_failed" "GUARD-REPEAT-FAIL" "$(basename "${REPORT_REPEAT}")"
  exit 1
fi

normalized_ok="$(mktemp)"
normalized_repeat="$(mktemp)"
tmp_fail_policy="$(mktemp)"
cleanup() {
  rm -f "${normalized_ok}" "${normalized_repeat}" "${tmp_fail_policy}"
}
trap cleanup EXIT

jq -S 'del(.checked_at)' "${REPORT_OK}" > "${normalized_ok}"
jq -S 'del(.checked_at)' "${REPORT_REPEAT}" > "${normalized_repeat}"
if cmp -s "${normalized_ok}" "${normalized_repeat}"; then
  emit_log "validation" "determinism.repeat_run" "compare=report_ok_vs_report_repeat" "passed" "repeat_run_stable" "none" "$(basename "${REPORT_REPEAT}")"
else
  emit_log "validation" "determinism.repeat_run" "compare=report_ok_vs_report_repeat" "failed" "repeat_run_drift" "DETERMINISM-DRIFT" "$(basename "${REPORT_REPEAT}")"
  exit 1
fi

jq '.forbidden_token_ceilings["tokio::"] = 0' "${POLICY}" > "${tmp_fail_policy}"

emit_log "validation" "failure_injection.token_ceiling" "mutate=tokio_ceiling_to_zero" "running" "none" "none" "$(basename "${tmp_fail_policy}")"
set +e
bash "${VALIDATOR}" --policy-path "${tmp_fail_policy}" --output "${REPORT_FAIL}" >> "${STDOUT_FILE}" 2>&1
failure_rc=$?
set -e
if [[ ${failure_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection.token_ceiling" "mutate=tokio_ceiling_to_zero" "failed" "failure_injection_not_detected" "EXPECTED-FAILURE-MISSING" "$(basename "${REPORT_FAIL}")"
  exit 1
fi

if ! jq -e '.status == "failed" and .error_code == "token_ceiling_exceeded"' "${REPORT_FAIL}" >/dev/null; then
  emit_log "validation" "failure_injection.token_ceiling" "mutate=tokio_ceiling_to_zero" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${REPORT_FAIL}")"
  exit 1
fi
emit_log "validation" "failure_injection.token_ceiling" "mutate=tokio_ceiling_to_zero" "passed" "expected_failure_detected" "none" "$(basename "${REPORT_FAIL}")"

emit_log "validation" "recovery_path" "run=canonical_policy_recheck" "running" "none" "none" "$(basename "${REPORT_RECOVERY}")"
if bash "${VALIDATOR}" --policy-path "${POLICY}" --output "${REPORT_RECOVERY}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "recovery_path" "run=canonical_policy_recheck" "passed" "recovery_validated" "none" "$(basename "${REPORT_RECOVERY}")"
else
  emit_log "validation" "recovery_path" "run=canonical_policy_recheck" "failed" "recovery_validator_failed" "RECOVERY-VALIDATOR-FAIL" "$(basename "${REPORT_RECOVERY}")"
  exit 1
fi

run_rch_step \
  "compile_guard_target" \
  "compile_time.guard" \
  "cargo_check_target=frankenterm-core::lib" \
  cargo check -p frankenterm-core --lib

run_rch_step \
  "runtime_guard_targeted_test" \
  "runtime.guard.integration" \
  "test=runtime_compat::tests::surface_contract_entries_are_unique(lib-only)" \
  cargo test -p frankenterm-core --lib runtime_compat::tests::surface_contract_entries_are_unique -- --nocapture

emit_log "summary" "nominal->determinism->failure_injection->recovery->rch_compile->rch_runtime" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
echo "ft-e34d9.10.8.2 cutover runtime guardrails scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
