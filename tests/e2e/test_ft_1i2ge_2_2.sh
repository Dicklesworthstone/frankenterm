#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_2_2_capability_availability_model"
CORRELATION_ID="ft-1i2ge.2.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_2_2_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_1i2ge_2_2_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_2_2_${RUN_ID}.probe.log"

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
    --arg component "mission_suitability.e2e" \
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
  "agent capability + availability suitability model validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log "failed" "preflight_jq" "jq_missing" "jq_not_found" "$(basename "${LOG_FILE}")" "jq required"
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log "failed" "preflight_rch" "rch_missing" "rch_not_found" "$(basename "${LOG_FILE}")" "rch required"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -eq 0 ]] && grep -q "✓" "${PROBE_FILE}"; then
  emit_log "running" "execution_preflight" "rch_workers_healthy" "none" "$(basename "${PROBE_FILE}")" "running through healthy rch workers"
else
  emit_log "running" "execution_preflight" "rch_workers_unavailable_fail_open" "none" "$(basename "${PROBE_FILE}")" "running through rch fail-open path"
fi

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_assignment_suitability_ -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_assignment_suitability_property_ -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="assignment_suitability_surface"
  reason_code="none"
  if [[ "${test_cmd}" == *"property_"* ]]; then
    decision_path="assignment_exclusion_property_path"
    reason_code="property_invariant_validation"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing: rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-2-2 ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "rch exec -- env CARGO_TARGET_DIR=target-rch-ft-1i2ge-2-2 ${test_cmd}"
  ) 2>&1 | tee -a "${STDOUT_FILE}"
  status=${PIPESTATUS[0]}
  set -e

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" \
      "exit=${status}; command=${test_cmd}"
    exit ${status}
  fi
done

required_markers=(
  "mission_assignment_suitability_prefers_lane_and_soft_capabilities ... ok"
  "mission_assignment_suitability_rejects_paused_and_rate_limited_agents ... ok"
  "mission_assignment_suitability_enforces_assignment_exclusions ... ok"
  "mission_assignment_suitability_handles_degraded_capacity_limits ... ok"
  "mission_assignment_suitability_rejects_unknown_candidate ... ok"
  "mission_assignment_suitability_property_excluded_agents_never_selected ... ok"
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
  "capability_profile->availability_gate->assignment_exclusion->suitability_scoring" \
  "assignment_suitability_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Capability and availability model validated with assignment exclusion enforcement and structured artifacts"
