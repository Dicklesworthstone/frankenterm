#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_3681t_4_4_robot_contracts"
CORRELATION_ID="ft-3681t.4.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_3681t_4_4_robot_contracts_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/ft_3681t_4_4_robot_contracts_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/ft_3681t_4_4_robot_contracts_${RUN_ID}.probe.log"
TARGET_DIR="target-rch-ft3681t44-contracts"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "3681t_4_4_robot_contracts"
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
    --arg component "robot_contracts.e2e" \
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

run_rch_test() {
  local decision_path="$1"
  local reason_code="$2"
  local cargo_cmd="$3"

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing via rch: ${cargo_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "rch exec -- env CARGO_TARGET_DIR=${TARGET_DIR} ${cargo_cmd}"
  ) 2>&1 | tee -a "${STDOUT_FILE}"
  local status=${PIPESTATUS[0]}
  set -e

  if [[ ${status} -ne 0 ]]; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "test_failure" \
      "cargo_test_failed" \
      "$(basename "${STDOUT_FILE}")" \
      "exit=${status}; command=${cargo_cmd}"
    exit "${status}"
  fi

  emit_log \
    "passed" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Completed via rch: ${cargo_cmd}"
}

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "robot contract artifact/export validation with rch offload"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for structured logging"
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "rch_required" \
    "rch_not_installed" \
    "$(basename "${LOG_FILE}")" \
    "rch is required for contract test execution"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all
) > "${PROBE_FILE}" 2>&1
probe_status=$?
set -e

if [[ ${probe_status} -eq 0 ]] && grep -q "✓" "${PROBE_FILE}"; then
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_healthy" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running robot contract validation through healthy rch workers"
else
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_unavailable_fail_open" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running robot contract validation through rch fail-open path"
fi

: > "${STDOUT_FILE}"

run_rch_test \
  "sdk_artifact_exports" \
  "sdk_bundle_validation" \
  "cargo test -p frankenterm-core --lib contract_artifact_bundle_ -- --nocapture"

run_rch_test \
  "api_contract_exports" \
  "contract_export_validation" \
  "cargo test -p frankenterm-core --lib contract_export_artifacts_ -- --nocapture"

run_rch_test \
  "contract_lifecycle_smoke" \
  "contract_lifecycle_validation" \
  "cargo test -p frankenterm-core --lib e2e_ -- --nocapture"

required_markers=(
  "contract_artifact_bundle_renders_deterministic_exports ... ok"
  "contract_artifact_bundle_sdk_sources_include_wire_keys ... ok"
  "contract_export_artifacts_render_json_snapshots ... ok"
  "contract_export_artifacts_preserve_failure_metadata ... ok"
  "e2e_sdk_generation_and_compat_validation ... ok"
  "e2e_replay_contract_suite ... ok"
  "e2e_full_contract_validation ... ok"
  "e2e_contract_with_failures_and_diffs ... ok"
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
  "sdk_exports->contract_exports->replay_validation" \
  "robot_contracts_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Robot contract artifact rendering, compatibility bundle export, and replay lifecycle validation completed"

