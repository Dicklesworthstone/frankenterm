#!/usr/bin/env bash
# ft-xbnl0.5.2 — Finish-line guard composition E2E.
#
# Exercises:
#   1. composition script runs and writes summary.json + structured.log
#   2. determinism: repeat run produces the same outcome (guards stay stable)
#   3. failure injection: mutate a manifest entry to point at a missing
#      script — composition must surface guard_script_missing
#   4. recovery: run against canonical manifest again, expect PASS
#   5. rch-backed cargo test of the Rust integration test (best-effort;
#      skipped under FT_XBNL0_5_2_SKIP_RCH=1)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_xbnl0_5_2_finish_line_guards"
CORRELATION_ID="ft-xbnl0.5.2-${RUN_ID}"
ARTIFACT_DIR="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"
LOG_FILE="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/scenario.stdout.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

REPORT_OK="${ARTIFACT_DIR}/report.nominal.json"
REPORT_REPEAT="${ARTIFACT_DIR}/report.repeat.json"
REPORT_FAIL="${ARTIFACT_DIR}/report.fail_injected.json"
REPORT_RECOVERY="${ARTIFACT_DIR}/report.recovery.json"

SCRIPT="${ROOT_DIR}/scripts/check_finish_line_guards.sh"
MANIFEST="${ROOT_DIR}/docs/ft-xbnl0-5-2-finish-line-guards.json"

# shellcheck disable=SC1091
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_5_2_finish_line_guards"

emit_log() {
  local component="$1" decision_path="$2" input_summary="$3"
  local outcome="$4" reason_code="$5" error_code="$6" artifact_path="$7"
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
      timestamp: $timestamp, component: $component,
      scenario_id: $scenario_id, correlation_id: $correlation_id,
      decision_path: $decision_path, input_summary: $input_summary,
      outcome: $outcome, reason_code: $reason_code,
      error_code: $error_code, artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

write_summary() {
  local outcome="$1"
  jq -n \
    --arg outcome "${outcome}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg structured_log "${LOG_FILE}" \
    --arg manifest_path "${MANIFEST}" \
    --arg script "${SCRIPT}" \
    '{
      scenario: "ft-xbnl0.5.2 finish-line guards composition",
      bead_id: "ft-xbnl0.5.2",
      run_id: $run_id,
      correlation_id: $correlation_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      structured_log: $structured_log,
      manifest_path: $manifest_path,
      script: $script
    }' > "${SUMMARY_FILE}"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    write_summary "failed"
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd python3
require_cmd bash

if [[ ! -f "${SCRIPT}" ]]; then
  emit_log "preflight" "required_artifacts" "script=${SCRIPT}" "failed" "missing_artifact" "ARTIFACT-MISSING" "${SCRIPT}"
  write_summary "failed"
  echo "composition script missing: ${SCRIPT}" >&2
  exit 1
fi
if [[ ! -f "${MANIFEST}" ]]; then
  emit_log "preflight" "required_artifacts" "manifest=${MANIFEST}" "failed" "missing_artifact" "ARTIFACT-MISSING" "${MANIFEST}"
  write_summary "failed"
  echo "composition manifest missing: ${MANIFEST}" >&2
  exit 1
fi

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

# Step 1: nominal run (cargo-test guard skipped to keep this scenario
# self-contained and fast; the Rust integration test covers the cargo
# path separately).
emit_log "validation" "composition.nominal" "skip_cargo=1" "running" "none" "none" "$(basename "${REPORT_OK}")"
set +e
FT_XBNL0_5_2_SKIP_CARGO_TEST=1 bash "${SCRIPT}" --output "${REPORT_OK}" >> "${STDOUT_FILE}" 2>&1
nominal_rc=$?
set -e
if [[ ${nominal_rc} -ne 0 ]]; then
  emit_log "validation" "composition.nominal" "skip_cargo=1" "failed" "composition_failed" "COMPOSITION-FAIL" "$(basename "${REPORT_OK}")"
  write_summary "failed"
  exit 1
fi
if ! jq -e '.status == "passed"' "${REPORT_OK}" >/dev/null; then
  emit_log "validation" "composition.nominal" "report_status_check" "failed" "nominal_status_not_passed" "REPORT-STATUS" "$(basename "${REPORT_OK}")"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "composition.nominal" "skip_cargo=1" "passed" "all_guards_passed" "none" "$(basename "${REPORT_OK}")"

# Step 2: determinism — repeat run must produce stable outcome.
emit_log "validation" "composition.repeat" "skip_cargo=1" "running" "none" "none" "$(basename "${REPORT_REPEAT}")"
set +e
FT_XBNL0_5_2_SKIP_CARGO_TEST=1 bash "${SCRIPT}" --output "${REPORT_REPEAT}" >> "${STDOUT_FILE}" 2>&1
repeat_rc=$?
set -e
if [[ ${repeat_rc} -ne 0 ]] || ! jq -e '.status == "passed"' "${REPORT_REPEAT}" >/dev/null; then
  emit_log "validation" "composition.repeat" "compare=nominal_vs_repeat" "failed" "repeat_failed" "REPEAT-FAIL" "$(basename "${REPORT_REPEAT}")"
  write_summary "failed"
  exit 1
fi

# Compare guard_id list + outcome list (ignore timestamps and log tails
# which legitimately vary run to run).
nominal_guards="$(jq -c '.guards | map({guard_id, outcome})' "${REPORT_OK}")"
repeat_guards="$(jq -c '.guards | map({guard_id, outcome})' "${REPORT_REPEAT}")"
if [[ "${nominal_guards}" != "${repeat_guards}" ]]; then
  emit_log "validation" "composition.repeat" "compare=guard_outcomes" "failed" "determinism_drift" "DETERMINISM-DRIFT" "$(basename "${REPORT_REPEAT}")"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "composition.repeat" "compare=guard_outcomes" "passed" "deterministic" "none" "$(basename "${REPORT_REPEAT}")"

# Step 3: failure injection — mutate a manifest entry to point at a
# missing script. The composition must surface the failure with an
# actionable reason_code.
mutated_manifest="$(mktemp)"
trap 'rm -f "${mutated_manifest}"' EXIT
jq '.guards |= map(
    if .guard_id == "no_runtime_regression" then
      .script = "scripts/does_not_exist_ft_xbnl0_5_2.sh"
    else
      .
    end
  )' "${MANIFEST}" > "${mutated_manifest}"

emit_log "validation" "composition.failure_injection" "mutate=missing_script" "running" "none" "none" "$(basename "${REPORT_FAIL}")"
set +e
FT_XBNL0_5_2_SKIP_CARGO_TEST=1 bash "${SCRIPT}" --manifest "${mutated_manifest}" --output "${REPORT_FAIL}" >> "${STDOUT_FILE}" 2>&1
fail_rc=$?
set -e
if [[ ${fail_rc} -eq 0 ]]; then
  emit_log "validation" "composition.failure_injection" "mutate=missing_script" "failed" "expected_failure_missing" "EXPECTED-FAILURE-MISSING" "$(basename "${REPORT_FAIL}")"
  write_summary "failed"
  exit 1
fi
if ! jq -e '.status == "failed" and (.guards | map(select(.guard_id == "no_runtime_regression")) | .[0].reason_code == "guard_script_missing")' "${REPORT_FAIL}" >/dev/null; then
  emit_log "validation" "composition.failure_injection" "mutate=missing_script" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE" "$(basename "${REPORT_FAIL}")"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "composition.failure_injection" "mutate=missing_script" "passed" "expected_failure_detected" "none" "$(basename "${REPORT_FAIL}")"

# Step 4: recovery — canonical manifest should still pass after the
# failure-injection run touched neither the real manifest nor the real
# guard scripts.
emit_log "validation" "composition.recovery" "skip_cargo=1" "running" "none" "none" "$(basename "${REPORT_RECOVERY}")"
set +e
FT_XBNL0_5_2_SKIP_CARGO_TEST=1 bash "${SCRIPT}" --output "${REPORT_RECOVERY}" >> "${STDOUT_FILE}" 2>&1
recovery_rc=$?
set -e
if [[ ${recovery_rc} -ne 0 ]] || ! jq -e '.status == "passed"' "${REPORT_RECOVERY}" >/dev/null; then
  emit_log "validation" "composition.recovery" "canonical_recheck" "failed" "recovery_failed" "RECOVERY-FAIL" "$(basename "${REPORT_RECOVERY}")"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "composition.recovery" "canonical_recheck" "passed" "recovery_passed" "none" "$(basename "${REPORT_RECOVERY}")"

# Step 5: rch-backed cargo test (best effort; opt out with FT_XBNL0_5_2_SKIP_RCH=1).
if [[ "${FT_XBNL0_5_2_SKIP_RCH:-0}" == "1" ]]; then
  emit_log "validation" "rch.cargo_test" "skip=env_opt_out" "skipped" "skip_via_FT_XBNL0_5_2_SKIP_RCH" "none" "n/a"
elif command -v rch >/dev/null 2>&1 && ensure_rch_ready 2>/dev/null; then
  BASE_CARGO_TARGET_DIR="target/rch-e2e-ft-xbnl0-5-2"
  if [[ -n "${CARGO_TARGET_DIR:-}" && "${CARGO_TARGET_DIR}" == target/* ]]; then
    BASE_CARGO_TARGET_DIR="${CARGO_TARGET_DIR}"
  fi
  export CARGO_TARGET_DIR="${BASE_CARGO_TARGET_DIR%/}-${RUN_ID}"
  step_log="${ARTIFACT_DIR}/rch_cargo_test.log"
  emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_5_2_finish_line_guards" "running" "none" "none" "$(basename "${step_log}")"
  set +e
  run_rch_cargo_logged "${step_log}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" \
    cargo test -p frankenterm-core --test ft_xbnl0_5_2_finish_line_guards -- --nocapture
  rch_rc=$?
  set -e
  if [[ ${rch_rc} -eq 0 ]]; then
    emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_5_2_finish_line_guards" "passed" "rch_cargo_test_passed" "none" "$(basename "${step_log}")"
  else
    emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_5_2_finish_line_guards" "failed" "rch_cargo_test_failed" "RCH-CARGO-TEST-FAIL" "$(basename "${step_log}")"
    write_summary "failed"
    exit 1
  fi
else
  emit_log "validation" "rch.cargo_test" "skip=rch_unavailable" "skipped" "rch_unavailable" "none" "n/a"
fi

emit_log "summary" "nominal->determinism->failure_injection->recovery->rch_cargo_test" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${SUMMARY_FILE}")"
write_summary "passed"
echo "ft-xbnl0.5.2 finish-line guards composition scenario PASSED."
echo "Artifacts: ${ARTIFACT_DIR}"
echo "  summary: ${SUMMARY_FILE}"
echo "  log:     ${LOG_FILE}"
