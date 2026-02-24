#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_contract"
CORRELATION_ID="ft-1i2ge.8.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.stdout.log"
PROBE_FILE="${LOG_DIR}/mission_tx_contract_${RUN_ID}.probe.log"

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
    --arg component "mission_tx_contract.e2e" \
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
  "mission tx contract validation (nominal + failure-injection + recovery)"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq required for structured logging"
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "rch_required" \
    "rch_not_installed" \
    "$(basename "${LOG_FILE}")" \
    "rch is required for all cargo test execution"
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
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_healthy" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running cargo tests through healthy rch workers"
else
  emit_log \
    "running" \
    "execution_preflight" \
    "rch_workers_unavailable_fail_open" \
    "none" \
    "$(basename "${PROBE_FILE}")" \
    "running cargo tests through rch fail-open path"
fi

TEST_CMDS=(
  "cargo test -p frankenterm-core --lib mission_tx_failure_taxonomy_has_unique_reason_and_error_codes -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_transition_table_rejects_illegal_edges -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_accepts_happy_path -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_accepts_recovery_path_with_compensation -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_rejects_non_monotonic_receipts -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_rejects_commit_without_prepared_receipt -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_rejects_double_commit_markers -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_rejects_compensation_without_commit_failure_marker -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_rejects_outcome_state_mismatch -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_transition_log_enforces_structured_contract -- --nocapture"
  "cargo test -p frankenterm-core --lib mission_tx_contract_property_ -- --nocapture"
)

: >"${STDOUT_FILE}"
for test_cmd in "${TEST_CMDS[@]}"; do
  decision_path="contract_surface"
  reason_code="none"
  if [[ "${test_cmd}" == *"accepts_happy_path"* ]]; then
    decision_path="nominal_path"
    reason_code="nominal_contract_path"
  elif [[ "${test_cmd}" == *"accepts_recovery_path"* ]]; then
    decision_path="recovery_path"
    reason_code="compensation_recovery_path"
  elif [[ "${test_cmd}" == *"rejects_"* ]]; then
    decision_path="failure_injection_path"
    reason_code="invariant_violation_rejected"
  elif [[ "${test_cmd}" == *"property_"* ]]; then
    decision_path="determinism_property_path"
    reason_code="property_invariant_validation"
  fi

  emit_log \
    "running" \
    "${decision_path}" \
    "${reason_code}" \
    "none" \
    "$(basename "${STDOUT_FILE}")" \
    "Executing: rch exec -- env CARGO_TARGET_DIR=target-rch-mission-tx-contract ${test_cmd}"

  set +e
  (
    cd "${ROOT_DIR}"
    eval "rch exec -- env CARGO_TARGET_DIR=target-rch-mission-tx-contract ${test_cmd}"
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
  "mission_tx_failure_taxonomy_has_unique_reason_and_error_codes ... ok"
  "mission_tx_transition_table_rejects_illegal_edges ... ok"
  "mission_tx_contract_accepts_happy_path ... ok"
  "mission_tx_contract_accepts_recovery_path_with_compensation ... ok"
  "mission_tx_contract_rejects_non_monotonic_receipts ... ok"
  "mission_tx_contract_rejects_commit_without_prepared_receipt ... ok"
  "mission_tx_contract_rejects_double_commit_markers ... ok"
  "mission_tx_contract_rejects_compensation_without_commit_failure_marker ... ok"
  "mission_tx_contract_rejects_outcome_state_mismatch ... ok"
  "mission_tx_transition_log_enforces_structured_contract ... ok"
  "mission_tx_contract_property_rejects_non_monotonic_receipt_suffix ... ok"
  "mission_tx_contract_property_rejects_duplicate_commit_markers ... ok"
  "mission_tx_contract_property_enforces_single_terminal_outcome ... ok"
  "mission_tx_contract_property_rejects_compensation_without_commit_partial_marker ... ok"
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
  "draft->planned->prepared->committing->committed|commit_partial->compensating->rolled_back|failure_injection_rejections" \
  "transaction_contract_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Transaction entities, lifecycle matrix, failure taxonomy, and invariant/property checks validated with structured logs"
