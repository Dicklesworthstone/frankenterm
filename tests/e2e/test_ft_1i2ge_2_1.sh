#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_2_1_readiness_resolver"
CORRELATION_ID="ft-1i2ge.2.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.jsonl"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.probe.log"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_2_1_${RUN_ID}.stdout.log"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
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

if ! command -v rch >/dev/null 2>&1; then
  emit_log "failed" "preflight_rch" "rch_missing" "rch_not_found" "$(basename "${LOG_FILE}")" "rch required"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" "$(basename "${LOG_FILE}")" "jq required"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -ne 0 ]] || ! grep -q "✓" "${PROBE_FILE}"; then
  emit_log \
    "failed" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" \
    "No healthy remote RCH worker available"
  exit 1
fi

TEST_CMDS=(
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-2-1 cargo test -p frankenterm-core beads_readiness_ -- --nocapture"
  "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-2-1 cargo test -p frankenterm-core test_readiness_report_ -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  emit_log "running" "cargo_test" "none" "none" "$(basename "${STDOUT_FILE}")" "Executing: ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "${test_cmd}"
  ) 2>&1 | tee -a "${STDOUT_FILE}"
  status=${PIPESTATUS[0]}
  set -e

  if grep -q "\[RCH\] local" "${STDOUT_FILE}"; then
    emit_log "failed" "cargo_test" "rch_fallback_local" "offload_contract_violation" "$(basename "${STDOUT_FILE}")" "rch fell back to local execution"
    exit 1
  fi

  if [[ ${status} -ne 0 ]]; then
    emit_log "failed" "cargo_test" "test_failure" "cargo_test_failed" "$(basename "${STDOUT_FILE}")" "exit=${status}"
    exit ${status}
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
