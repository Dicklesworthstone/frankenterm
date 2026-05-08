#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_1_3_failure_taxonomy"
CORRELATION_ID="ft-1i2ge.1.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_1_3_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_1_3_${RUN_ID}.stdout.log"

GUARD_LIB="$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_1_3"
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
    --arg component "mission_failure_taxonomy.e2e" \
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
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "mission failure taxonomy contract validation (nominal + failure-injection + recovery)"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

RCH_TARGET_DIR="target-rch-ft-1i2ge-1-3"
TEST_FILTERS=(
  "mission_failure_taxonomy_"
  "mission_validate_rejects_unknown_failure_reason_code"
  "mission_validate_rejects_mismatched_failure_error_code"
  "mission_validate_accepts_recoverable_failure_contract"
  "mission_validate_requires_canonical_approval_"
  "mission_validate_rejects_escalation_error_without_canonical_reason"
)

: >"${STDOUT_FILE}"
command_index=0
for test_filter in "${TEST_FILTERS[@]}"; do
  command_index=$((command_index + 1))
  step_stdout="${LOG_DIR}/ft_1i2ge_1_3_${RUN_ID}.step_${command_index}.stdout.log"
  test_cmd=(env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core --lib "${test_filter}" -- --nocapture)
  decision_path="contract_surface"
  reason_code="none"
  if [[ "${test_filter}" == *"rejects_"* ]]; then
    decision_path="failure_injection_path"
    reason_code="failure_contract_rejection_checks"
  elif [[ "${test_filter}" == *"accepts_recoverable"* ]]; then
    decision_path="recovery_path"
    reason_code="recoverable_contract_acceptance"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing through rch: ${test_cmd[*]}"

  set +e
  run_rch_cargo_logged "${step_stdout}" "${test_cmd[@]}"
  status=$?
  set -e
  cat "${step_stdout}" >>"${STDOUT_FILE}"

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" \
      "exit=${status}; command=${test_cmd[*]}"
    exit "${status}"
  fi
done

required_markers=(
  "mission_failure_taxonomy_catalog_has_unique_reason_and_error_codes ... ok"
  "mission_failure_taxonomy_marks_retryability_and_hints ... ok"
  "mission_validate_rejects_unknown_failure_reason_code ... ok"
  "mission_validate_rejects_mismatched_failure_error_code ... ok"
  "mission_validate_accepts_recoverable_failure_contract ... ok"
  "mission_validate_requires_canonical_approval_denied_reason ... ok"
  "mission_validate_requires_canonical_approval_expired_reason ... ok"
  "mission_validate_rejects_escalation_error_without_canonical_reason ... ok"
)

for marker in "${required_markers[@]}"; do
  if ! grep -q "${marker}" "${STDOUT_FILE}"; then
    emit_log \
      "failed" \
      "assertion_check" \
      "missing_success_marker" \
      "expected_test_marker_missing" \
      "$(basename "${STDOUT_FILE}")" \
      "Missing marker: ${marker}"
    exit 1
  fi
done

emit_log \
  "passed" \
  "failure_taxonomy_catalog->failure_injection_path->recovery_path" \
  "failure_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Mission failure taxonomy reason/error contract validated with explicit retryability + remediation hints"
