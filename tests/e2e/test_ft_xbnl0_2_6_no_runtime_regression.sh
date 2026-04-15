#!/usr/bin/env bash
# ft-xbnl0.2.6 — Strict no-runtime-regression gate E2E.
#
# Exercises:
#   1. validator self-tests pass against synthetic mini-repos
#   2. validator passes against the canonical policy (nominal run)
#   3. determinism: repeat run produces byte-identical report (modulo timestamp)
#   4. failure injection — strip an entry from source_quarantine and assert
#      the gate now fails with `source_runtime_regression`
#   5. failure injection — drop a manifest from manifest_dependency_allowlist
#      and assert the gate fails with `manifest_dependency_regression`
#   6. failure injection — add an unallowed doc and assert the gate fails
#      with `doc_narrative_regression`
#   7. recovery — re-running against the canonical policy passes again
#   8. rch-backed cargo test of the Rust integration test exercising the gate
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_xbnl0_2_6_no_runtime_regression"
CORRELATION_ID="ft-xbnl0.2.6-${RUN_ID}"
ARTIFACT_DIR="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}"
mkdir -p "${ARTIFACT_DIR}"
LOG_FILE="${ARTIFACT_DIR}/structured.log"
STDOUT_FILE="${ARTIFACT_DIR}/scenario.stdout.log"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"

REPORT_OK="${ARTIFACT_DIR}/report.nominal.json"
REPORT_REPEAT="${ARTIFACT_DIR}/report.repeat.json"
REPORT_FAIL_SRC="${ARTIFACT_DIR}/report.fail_src.json"
REPORT_FAIL_DEP="${ARTIFACT_DIR}/report.fail_dep.json"
REPORT_FAIL_DOC="${ARTIFACT_DIR}/report.fail_doc.json"
REPORT_RECOVERY="${ARTIFACT_DIR}/report.recovery.json"

VALIDATOR="${ROOT_DIR}/scripts/check_no_runtime_regression.sh"
POLICY="${ROOT_DIR}/docs/ft-xbnl0-2-6-no-runtime-regression-gate.json"

BASE_CARGO_TARGET_DIR="target/rch-e2e-ft-xbnl0-2-6"
if [[ -n "${CARGO_TARGET_DIR:-}" && "${CARGO_TARGET_DIR}" == target/* ]]; then
  BASE_CARGO_TARGET_DIR="${CARGO_TARGET_DIR}"
fi
CARGO_TARGET_DIR="${BASE_CARGO_TARGET_DIR%/}-${RUN_ID}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""

# shellcheck disable=SC1091
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_xbnl0_2_6_no_runtime_regression"

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

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    write_summary "failed"
    exit 1
  fi
}

write_summary() {
  local outcome="$1"
  jq -n \
    --arg outcome "${outcome}" \
    --arg run_id "${RUN_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    --arg structured_log "${LOG_FILE}" \
    --arg policy_path "${POLICY}" \
    --arg validator "${VALIDATOR}" \
    '{
      scenario: "ft-xbnl0.2.6 no-runtime regression gate",
      bead_id: "ft-xbnl0.2.6",
      run_id: $run_id,
      correlation_id: $correlation_id,
      outcome: $outcome,
      artifact_dir: $artifact_dir,
      structured_log: $structured_log,
      policy_path: $policy_path,
      validator: $validator
    }' > "${SUMMARY_FILE}"
}

run_step() {
  local label="$1"
  shift
  LAST_STEP_LOG="${ARTIFACT_DIR}/${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return "${rc}"
}

run_rch_cargo_step() {
  local label="$1"
  shift
  LAST_STEP_LOG="${ARTIFACT_DIR}/${label}.log"
  set +e
  ( run_rch_cargo_logged "${LAST_STEP_LOG}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@" )
  local rc=$?
  set -e
  if [[ -f "${LAST_STEP_LOG}" ]]; then
    tee -a "${STDOUT_FILE}" < "${LAST_STEP_LOG}" >/dev/null
  fi
  return "${rc}"
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd python3
require_cmd cargo
require_cmd bash

if [[ ! -f "${VALIDATOR}" ]]; then
  emit_log "preflight" "required_artifacts" "validator=${VALIDATOR}" "failed" "missing_artifact" "ARTIFACT-MISSING" "${VALIDATOR}"
  write_summary "failed"
  echo "validator missing: ${VALIDATOR}" >&2
  exit 1
fi
if [[ ! -f "${POLICY}" ]]; then
  emit_log "preflight" "required_artifacts" "policy=${POLICY}" "failed" "missing_artifact" "ARTIFACT-MISSING" "${POLICY}"
  write_summary "failed"
  echo "policy missing: ${POLICY}" >&2
  exit 1
fi

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

# Step 1: validator self-tests
emit_log "validation" "validator.self_test" "self_test=enabled" "running" "none" "none" "$(basename "${REPORT_OK}")"
if bash "${VALIDATOR}" --self-test --policy-path "${POLICY}" --output "${REPORT_OK}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "validator.self_test" "self_test=enabled" "passed" "self_test_passed" "none" "$(basename "${REPORT_OK}")"
else
  emit_log "validation" "validator.self_test" "self_test=enabled" "failed" "self_test_failed" "GATE-SELFTEST-FAIL" "$(basename "${REPORT_OK}")"
  write_summary "failed"
  exit 1
fi

if ! jq -e '.status == "passed"' "${REPORT_OK}" >/dev/null; then
  emit_log "validation" "validator.nominal" "report_status_check" "failed" "report_status_not_passed" "GATE-REPORT-STATUS" "$(basename "${REPORT_OK}")"
  write_summary "failed"
  exit 1
fi

# Step 2: determinism — repeat run yields byte-identical report (modulo timestamp).
emit_log "validation" "validator.repeat" "run=repeat" "running" "none" "none" "$(basename "${REPORT_REPEAT}")"
if bash "${VALIDATOR}" --policy-path "${POLICY}" --output "${REPORT_REPEAT}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "validator.repeat" "run=repeat" "passed" "repeat_passed" "none" "$(basename "${REPORT_REPEAT}")"
else
  emit_log "validation" "validator.repeat" "run=repeat" "failed" "repeat_failed" "GATE-REPEAT-FAIL" "$(basename "${REPORT_REPEAT}")"
  write_summary "failed"
  exit 1
fi

normalized_ok="$(mktemp)"; normalized_repeat="$(mktemp)"
trap 'rm -f "${normalized_ok}" "${normalized_repeat}"' EXIT
jq -S 'del(.checked_at)' "${REPORT_OK}"     > "${normalized_ok}"
jq -S 'del(.checked_at)' "${REPORT_REPEAT}" > "${normalized_repeat}"
if cmp -s "${normalized_ok}" "${normalized_repeat}"; then
  emit_log "validation" "determinism" "compare=nominal_vs_repeat" "passed" "deterministic" "none" "$(basename "${REPORT_REPEAT}")"
else
  emit_log "validation" "determinism" "compare=nominal_vs_repeat" "failed" "drift_detected" "DETERMINISM-DRIFT" "$(basename "${REPORT_REPEAT}")"
  write_summary "failed"
  exit 1
fi

# Step 3: failure injection — strip a source_quarantine entry → expect source_runtime_regression
mutated_src="$(mktemp)"
jq 'del(.source_quarantine."crates/frankenterm-core/src/runtime_compat.rs")' "${POLICY}" > "${mutated_src}"
emit_log "validation" "failure_injection.source" "mutate=drop_runtime_compat_quarantine" "running" "none" "none" "$(basename "${REPORT_FAIL_SRC}")"
set +e
bash "${VALIDATOR}" --policy-path "${mutated_src}" --output "${REPORT_FAIL_SRC}" >> "${STDOUT_FILE}" 2>&1
src_rc=$?
set -e
if [[ ${src_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection.source" "mutate=drop_runtime_compat_quarantine" "failed" "expected_failure_missing" "EXPECTED-FAILURE-MISSING" "$(basename "${REPORT_FAIL_SRC}")"
  rm -f "${mutated_src}"
  write_summary "failed"
  exit 1
fi
if ! jq -e '.status == "failed" and .error_code == "source_runtime_regression"' "${REPORT_FAIL_SRC}" >/dev/null; then
  emit_log "validation" "failure_injection.source" "mutate=drop_runtime_compat_quarantine" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${REPORT_FAIL_SRC}")"
  rm -f "${mutated_src}"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "failure_injection.source" "mutate=drop_runtime_compat_quarantine" "passed" "expected_failure_detected" "none" "$(basename "${REPORT_FAIL_SRC}")"
rm -f "${mutated_src}"

# Step 4: failure injection — strip a manifest allowlist entry → expect manifest_dependency_regression
mutated_dep="$(mktemp)"
jq 'del(.manifest_dependency_allowlist.tokio."Cargo.toml")' "${POLICY}" > "${mutated_dep}"
emit_log "validation" "failure_injection.manifest" "mutate=drop_workspace_tokio_allow" "running" "none" "none" "$(basename "${REPORT_FAIL_DEP}")"
set +e
bash "${VALIDATOR}" --policy-path "${mutated_dep}" --output "${REPORT_FAIL_DEP}" >> "${STDOUT_FILE}" 2>&1
dep_rc=$?
set -e
if [[ ${dep_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection.manifest" "mutate=drop_workspace_tokio_allow" "failed" "expected_failure_missing" "EXPECTED-FAILURE-MISSING" "$(basename "${REPORT_FAIL_DEP}")"
  rm -f "${mutated_dep}"
  write_summary "failed"
  exit 1
fi
if ! jq -e '.status == "failed" and .error_code == "manifest_dependency_regression"' "${REPORT_FAIL_DEP}" >/dev/null; then
  emit_log "validation" "failure_injection.manifest" "mutate=drop_workspace_tokio_allow" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${REPORT_FAIL_DEP}")"
  rm -f "${mutated_dep}"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "failure_injection.manifest" "mutate=drop_workspace_tokio_allow" "passed" "expected_failure_detected" "none" "$(basename "${REPORT_FAIL_DEP}")"
rm -f "${mutated_dep}"

# Step 5: failure injection — drop a doc_narrative_allowlist entry → expect doc_narrative_regression
mutated_doc="$(mktemp)"
jq '.doc_narrative_allowlist |= map(select(. != "docs/timing-determinism.md"))' "${POLICY}" > "${mutated_doc}"
emit_log "validation" "failure_injection.doc" "mutate=drop_timing_determinism_allow" "running" "none" "none" "$(basename "${REPORT_FAIL_DOC}")"
set +e
bash "${VALIDATOR}" --policy-path "${mutated_doc}" --output "${REPORT_FAIL_DOC}" >> "${STDOUT_FILE}" 2>&1
doc_rc=$?
set -e
if [[ ${doc_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection.doc" "mutate=drop_timing_determinism_allow" "failed" "expected_failure_missing" "EXPECTED-FAILURE-MISSING" "$(basename "${REPORT_FAIL_DOC}")"
  rm -f "${mutated_doc}"
  write_summary "failed"
  exit 1
fi
if ! jq -e '.status == "failed" and .error_code == "doc_narrative_regression"' "${REPORT_FAIL_DOC}" >/dev/null; then
  emit_log "validation" "failure_injection.doc" "mutate=drop_timing_determinism_allow" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${REPORT_FAIL_DOC}")"
  rm -f "${mutated_doc}"
  write_summary "failed"
  exit 1
fi
emit_log "validation" "failure_injection.doc" "mutate=drop_timing_determinism_allow" "passed" "expected_failure_detected" "none" "$(basename "${REPORT_FAIL_DOC}")"
rm -f "${mutated_doc}"

# Step 6: recovery — canonical policy still passes after failure injections
emit_log "validation" "recovery_path" "run=canonical_recheck" "running" "none" "none" "$(basename "${REPORT_RECOVERY}")"
if bash "${VALIDATOR}" --policy-path "${POLICY}" --output "${REPORT_RECOVERY}" >> "${STDOUT_FILE}" 2>&1; then
  emit_log "validation" "recovery_path" "run=canonical_recheck" "passed" "recovery_passed" "none" "$(basename "${REPORT_RECOVERY}")"
else
  emit_log "validation" "recovery_path" "run=canonical_recheck" "failed" "recovery_failed" "RECOVERY-FAIL" "$(basename "${REPORT_RECOVERY}")"
  write_summary "failed"
  exit 1
fi

# Step 7: rch-backed cargo test of the Rust integration test that re-invokes the gate.
# Honors FT_XBNL0_2_6_SKIP_RCH=1 for environments where the rch worker substrate is
# unavailable or known to be slow; the artifact bundle still records the skip reason.
if [[ "${FT_XBNL0_2_6_SKIP_RCH:-0}" == "1" ]]; then
  emit_log "validation" "rch.cargo_test" "skip=env_opt_out" "skipped" "skip_via_FT_XBNL0_2_6_SKIP_RCH" "none" "n/a"
elif command -v rch >/dev/null 2>&1 && ensure_rch_ready 2>/dev/null; then
  emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_2_6_no_runtime_regression_gate" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_rch_cargo_step "rch_cargo_test" \
      test -p frankenterm-core --test ft_xbnl0_2_6_no_runtime_regression_gate -- --nocapture; then
    emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_2_6_no_runtime_regression_gate" "passed" "rch_cargo_test_passed" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "rch.cargo_test" "test=ft_xbnl0_2_6_no_runtime_regression_gate" "failed" "rch_cargo_test_failed" "RCH-CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
    write_summary "failed"
    exit 1
  fi
else
  emit_log "validation" "rch.cargo_test" "skip=rch_unavailable" "skipped" "rch_unavailable" "none" "n/a"
fi

emit_log "summary" "self_test->nominal->determinism->failure_injection_src->failure_injection_dep->failure_injection_doc->recovery->rch_cargo_test" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${SUMMARY_FILE}")"
write_summary "passed"
echo "ft-xbnl0.2.6 no-runtime regression gate scenario PASSED."
echo "Artifacts: ${ARTIFACT_DIR}"
echo "  summary: ${SUMMARY_FILE}"
echo "  log:     ${LOG_FILE}"
