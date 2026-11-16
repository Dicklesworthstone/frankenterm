#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_2_1_readiness_resolver"
CORRELATION_ID="ft-1i2ge.2.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.stdout.log"

GUARD_LIB="$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_2_1"
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
    --arg component "beads_readiness.e2e" \
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
  "beads DAG ingestion + readiness resolver validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" "$(basename "${LOG_FILE}")" "jq required"
  exit 1
fi

DEFAULT_RCH_TARGET_DIR="target/rch-e2e-ft-1i2ge-2-1-${RUN_ID}"
REQUESTED_RCH_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${REQUESTED_RCH_TARGET_DIR}" && "${REQUESTED_RCH_TARGET_DIR}" != /* ]]; then
  RCH_TARGET_DIR="${REQUESTED_RCH_TARGET_DIR}"
else
  RCH_TARGET_DIR="${DEFAULT_RCH_TARGET_DIR}"
fi
TEST_FILTERS=(
  "beads_readiness_"
  "test_readiness_report_"
)

: >"${STDOUT_FILE}"
command_index=0
for test_filter in "${TEST_FILTERS[@]}"; do
  command_index=$((command_index + 1))
  step_stdout="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.step_${command_index}.stdout.log"
  test_cmd=(env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo test -p frankenterm-core "${test_filter}" -- --nocapture)
  emit_log "running" "cargo_test" "none" "none" "$(basename "${STDOUT_FILE}")" "Executing through rch: ${test_cmd[*]}"

  set +e
  run_rch_cargo_logged "${step_stdout}" "${test_cmd[@]}"
  status=$?
  set -e
  cat "${step_stdout}" >>"${STDOUT_FILE}"

  if [[ ${status} -ne 0 ]]; then
    emit_log "failed" "cargo_test" "test_failure" "cargo_test_failed" "$(basename "${STDOUT_FILE}")" "exit=${status}"
    exit "${status}"
  fi

done

required_markers=(
  "beads_readiness_resolver_marks_ready_when_blockers_closed ... ok"
  "beads_readiness_resolver_marks_missing_dependency_as_degraded ... ok"
  "test_readiness_report_from_details_produces_ready_ids ... ok"
)

for marker in "${required_markers[@]}"; do
  if ! grep -q "${marker}" "${STDOUT_FILE}"; then
    emit_log "failed" "assertion_check" "missing_success_marker" "expected_test_marker_missing" "$(basename "${STDOUT_FILE}")" "Missing marker: ${marker}"
    exit 1
  fi
done

emit_log \
  "passed" \
  "resolve_bead_readiness->readiness_report" \
  "resolver_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "DAG readiness resolver validated with structured hints and degraded-mode codes"
