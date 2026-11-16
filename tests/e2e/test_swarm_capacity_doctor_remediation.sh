#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="${SWARM_CAPACITY_DOCTOR_BEAD_ID:-ft-b94bx.9}"
RUN_ID="${SWARM_CAPACITY_DOCTOR_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_ROOT="${ROOT_DIR}/tests/e2e/artifacts/goal-line/${BEAD_ID}/swarm_capacity_doctor/${RUN_ID}"
LOG_FILE="${ARTIFACT_ROOT}/events.jsonl"
FIXTURE_FILE="${ROOT_DIR}/crates/frankenterm/tests/fixtures/golden_artifacts/swarm_capacity_operator/doctor-remediation.json"
RUNBOOK_FILE="${ROOT_DIR}/docs/operator-runbook.md"
CONTRACT_FILE="${ROOT_DIR}/docs/robot-contracts/swarm-capacity.md"

mkdir -p "${ARTIFACT_ROOT}"
: >"${LOG_FILE}"

emit_event() {
  local state_id="$1"
  local step="$2"
  local outcome="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"

  jq -cn \
    --arg timestamp "$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    --arg bead_id "${BEAD_ID}" \
    --arg run_id "${RUN_ID}" \
    --arg state_id "${state_id}" \
    --arg step "${step}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path#"${ROOT_DIR}/"}" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      run_id: $run_id,
      state_id: $state_id,
      step: $step,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path,
      cargo_reached: false,
      rustc_reached: false,
      test_execution_reached: false
    }' >>"${LOG_FILE}"
}

fail_step() {
  local state_id="$1"
  local step="$2"
  local reason_code="$3"
  local error_code="$4"
  local artifact_path="$5"
  emit_event "${state_id}" "${step}" "failed" "${reason_code}" "${error_code}" "${artifact_path}"
  exit 1
}

require_command() {
  local command_name="$1"
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    fail_step "${command_name}" "preflight" "capacity.doctor.tool_missing" "${command_name}_not_found" "${LOG_FILE}"
  fi
}

require_file() {
  local path="$1"
  local label="$2"
  if [[ ! -f "${path}" ]]; then
    fail_step "${label}" "preflight" "capacity.doctor.artifact_missing" "missing_artifact" "${path}"
  fi
}

emit_event "suite" "start" "running" "capacity.doctor.started" "none" "${LOG_FILE}"

require_command jq
require_file "${FIXTURE_FILE}" "fixture"
require_file "${RUNBOOK_FILE}" "runbook"
require_file "${CONTRACT_FILE}" "contract"

jq empty "${FIXTURE_FILE}"
emit_event "fixture" "jq_empty" "passed" "capacity.doctor.fixture_json" "none" "${FIXTURE_FILE}"

if [[ "$(jq -r '.contract_id' "${FIXTURE_FILE}")" != "ft.robot.swarm_capacity.operator.v1" ]]; then
  fail_step "fixture" "contract_id" "capacity.doctor.contract_invalid" "contract_id_invalid" "${FIXTURE_FILE}"
fi

if ! jq -e '.dry_run == true and .side_effects_executed == false and .live_mutation_allowed == false and .raw_pane_content_stored == false' "${FIXTURE_FILE}" >/dev/null; then
  fail_step "fixture" "safety_flags" "capacity.doctor.safety_flags_invalid" "safety_flags_invalid" "${FIXTURE_FILE}"
fi
emit_event "fixture" "safety_flags" "passed" "capacity.doctor.safety_flags" "none" "${FIXTURE_FILE}"

required_states=(stale_telemetry capacity_refused target_class_unavailable resource_pressure)
for state in "${required_states[@]}"; do
  if ! jq -e --arg state "${state}" '.remediation_states[] | select(.state == $state)' "${FIXTURE_FILE}" >/dev/null; then
    fail_step "${state}" "state_coverage" "capacity.doctor.state_missing" "state_missing" "${FIXTURE_FILE}"
  fi
  if ! grep -Fq "${state}" "${RUNBOOK_FILE}" || ! grep -Fq "${state}" "${CONTRACT_FILE}"; then
    fail_step "${state}" "doc_state_coverage" "capacity.doctor.doc_state_missing" "doc_state_missing" "${RUNBOOK_FILE}"
  fi
  emit_event "${state}" "state_coverage" "passed" "capacity.doctor.state_present" "none" "${FIXTURE_FILE}"
done

bad_state_count="$(jq '[.remediation_states[] | select(((.reason_codes // []) | length == 0) or ((.safe_actions // []) | length == 0) or (((.forbidden_actions // []) | index("local_cargo_proof") == null) and .state != "resource_pressure"))] | length' "${FIXTURE_FILE}")"
if [[ "${bad_state_count}" -ne 0 ]]; then
  fail_step "fixture" "state_fields" "capacity.doctor.state_fields_invalid" "state_fields_invalid" "${FIXTURE_FILE}"
fi
emit_event "fixture" "state_fields" "passed" "capacity.doctor.state_fields" "none" "${FIXTURE_FILE}"

unsafe_safe_actions="$(jq '[.remediation_states[] | select(((.safe_actions // []) + (.command_examples // [])) | map(tostring) | join(" ") | test("service_restart|agent_mail_repair|rch_worker_mutation|local_cargo_proof|spawn_panes|cancel_build|drain_worker|delete_files|claim_high_scale_capacity"))] | length' "${FIXTURE_FILE}")"
if [[ "${unsafe_safe_actions}" -ne 0 ]]; then
  fail_step "fixture" "safe_action_scan" "capacity.doctor.forbidden_action_recommended" "forbidden_action_recommended" "${FIXTURE_FILE}"
fi
emit_event "fixture" "safe_action_scan" "passed" "capacity.doctor.no_forbidden_recommendations" "none" "${FIXTURE_FILE}"

while IFS= read -r rel_path; do
  if [[ ! -e "${ROOT_DIR}/${rel_path}" ]]; then
    fail_step "artifact_ref" "path_exists" "capacity.doctor.artifact_ref_missing" "artifact_ref_missing" "${rel_path}"
  fi
done < <(jq -r '.remediation_states[].artifact_refs[].path' "${FIXTURE_FILE}" | sort -u)
emit_event "artifact_ref" "path_exists" "passed" "capacity.doctor.artifact_refs_exist" "none" "${FIXTURE_FILE}"

shared_needles=(
  "ft robot swarm-capacity status --format json --level 3" \
  "ft robot swarm-capacity plan --add-panes 12 --format json --level 3" \
  "ft robot swarm-capacity explain <decision-id> --format json" \
  "ft doctor --json" \
  "ft status --health" \
  "docs/attestations/perf/swarm-capacity-envelope.json" \
  "docs/perf/target-class-hardware.md" \
  "tests/e2e/test_swarm_capacity_doctor_remediation.sh"
)

for needle in "${shared_needles[@]}"; do
  if ! grep -Fq "${needle}" "${RUNBOOK_FILE}"; then
    fail_step "runbook" "needle_present" "capacity.doctor.runbook_missing" "doc_missing" "${RUNBOOK_FILE}"
  fi
  if ! grep -Fq "${needle}" "${CONTRACT_FILE}"; then
    fail_step "contract" "needle_present" "capacity.doctor.contract_missing" "doc_missing" "${CONTRACT_FILE}"
  fi
done
emit_event "doc" "needle_present" "passed" "capacity.doctor.doc_links" "none" "${RUNBOOK_FILE}"

for needle in \
  "Local-only swarm example" \
  "RCH-assisted build-heavy swarm example" \
  "Swarm-capacity: contract ft.robot.swarm_capacity.operator.v1"; do
  if ! grep -Fq "${needle}" "${RUNBOOK_FILE}"; then
    fail_step "runbook" "example_present" "capacity.doctor.runbook_example_missing" "doc_missing" "${RUNBOOK_FILE}"
  fi
done
emit_event "runbook" "example_present" "passed" "capacity.doctor.runbook_examples" "none" "${RUNBOOK_FILE}"

emit_event "suite" "finish" "passed" "capacity.doctor.completed" "none" "${LOG_FILE}"
printf 'swarm capacity doctor remediation: static verifier passed (%s states)\n' "${#required_states[@]}"
