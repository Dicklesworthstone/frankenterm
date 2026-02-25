#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_1_4_rch_policy"
CORRELATION_ID="ft-e34d9.10.1.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_rch_policy_${RUN_ID}.jsonl"

VALIDATOR="${ROOT_DIR}/scripts/validate_asupersync_rch_execution_policy.sh"
POLICY_DOC="${ROOT_DIR}/docs/asupersync-rch-execution-policy.md"
SCHEMA_DOC="${ROOT_DIR}/docs/asupersync-rch-evidence-schema.json"

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "asupersync_rch_policy.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
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
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.4 execution policy validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required"
  exit 1
fi

for artifact in "${VALIDATOR}" "${POLICY_DOC}" "${SCHEMA_DOC}"; do
  if [[ ! -f "${artifact}" ]]; then
    emit_log \
      "failed" \
      "suite_init" \
      "preflight_artifacts" \
      "missing_artifact" \
      "artifact_not_found" \
      "${artifact}" \
      "required policy artifact missing"
    exit 1
  fi
done

if [[ ! -x "${VALIDATOR}" ]]; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_validator" \
    "validator_not_executable" \
    "invalid_permissions" \
    "$(basename "${VALIDATOR}")" \
    "validator is not executable"
  exit 1
fi

emit_log \
  "running" \
  "unit_classifier" \
  "command_classification" \
  "none" \
  "none" \
  "$(basename "${VALIDATOR}")" \
  "validating heavy/light classifier behavior"

heavy_no_rch="$("${VALIDATOR}" --classify "cargo test --workspace")"
if [[ "$(jq -r '.is_heavy' <<<"${heavy_no_rch}")" != "true" || "$(jq -r '.policy_violation' <<<"${heavy_no_rch}")" != "true" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "cargo test should be heavy and policy violation without rch"
  exit 1
fi

light_cmd="$("${VALIDATOR}" --classify "cargo fmt --check")"
if [[ "$(jq -r '.is_heavy' <<<"${light_cmd}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "cargo fmt --check should be light"
  exit 1
fi

emit_log \
  "passed" \
  "unit_classifier" \
  "command_classification" \
  "classifier_validated" \
  "none" \
  "$(basename "${VALIDATOR}")" \
  "classifier behavior validated"

tmp_valid="$(mktemp)"
tmp_invalid="$(mktemp)"
tmp_recovery="$(mktemp)"
cleanup() {
  rm -f "${tmp_valid}" "${tmp_invalid}" "${tmp_recovery}"
}
trap cleanup EXIT

cat > "${tmp_valid}" <<'JSON'
{
  "schema_version": 1,
  "bead_id": "ft-e34d9.10.1.4",
  "policy_version": "1.0.0",
  "runs": [
    {
      "timestamp": "2026-02-25T00:00:00Z",
      "command": "rch exec -- cargo check --workspace --all-targets",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=contabo-2",
      "artifact_paths": ["tests/e2e/logs/mock_rch_policy.jsonl"],
      "elapsed_seconds": 31.4,
      "exit_status": 0,
      "residual_risk_notes": ""
    },
    {
      "timestamp": "2026-02-25T00:01:00Z",
      "command": "cargo fmt --check",
      "is_heavy": false,
      "used_rch": false,
      "worker_context": "local",
      "artifact_paths": ["tests/e2e/logs/mock_rch_policy.jsonl"],
      "elapsed_seconds": 0.6,
      "exit_status": 0,
      "residual_risk_notes": ""
    }
  ]
}
JSON

emit_log \
  "running" \
  "integration_valid_evidence" \
  "validate_evidence_schema" \
  "none" \
  "none" \
  "$(basename "${tmp_valid}")" \
  "valid evidence should pass policy validation"

if ! "${VALIDATOR}" --validate-evidence "${tmp_valid}" >/dev/null; then
  emit_log \
    "failed" \
    "integration_valid_evidence" \
    "validate_evidence_schema" \
    "unexpected_valid_reject" \
    "validator_rejected_valid_evidence" \
    "$(basename "${tmp_valid}")" \
    "valid evidence was rejected"
  exit 1
fi

emit_log \
  "passed" \
  "integration_valid_evidence" \
  "validate_evidence_schema" \
  "valid_evidence_accepted" \
  "none" \
  "$(basename "${tmp_valid}")" \
  "valid evidence accepted"

jq '.runs[0].command = "cargo test --workspace" | .runs[0].used_rch = false' "${tmp_valid}" > "${tmp_invalid}"

emit_log \
  "running" \
  "failure_injection" \
  "heavy_without_rch" \
  "none" \
  "none" \
  "$(basename "${tmp_invalid}")" \
  "heavy local run without fallback metadata should fail"

if "${VALIDATOR}" --validate-evidence "${tmp_invalid}" >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "failure_injection" \
    "heavy_without_rch" \
    "guardrail_not_enforced" \
    "unexpected_negative_pass" \
    "$(basename "${tmp_invalid}")" \
    "invalid evidence unexpectedly passed"
  exit 1
fi

emit_log \
  "passed" \
  "failure_injection" \
  "heavy_without_rch" \
  "negative_guardrail_enforced" \
  "none" \
  "$(basename "${tmp_invalid}")" \
  "invalid evidence correctly rejected"

jq '.runs[0].fallback_reason_code = "RCH-E100" | .runs[0].fallback_approved_by = "human-operator"' "${tmp_invalid}" > "${tmp_recovery}"

emit_log \
  "running" \
  "recovery_validation" \
  "fallback_metadata_present" \
  "none" \
  "none" \
  "$(basename "${tmp_recovery}")" \
  "fallback metadata should allow controlled heavy local fallback"

if ! "${VALIDATOR}" --validate-evidence "${tmp_recovery}" >/dev/null; then
  emit_log \
    "failed" \
    "recovery_validation" \
    "fallback_metadata_present" \
    "unexpected_recovery_fail" \
    "validator_rejected_recovery" \
    "$(basename "${tmp_recovery}")" \
    "recovery evidence should have passed"
  exit 1
fi

emit_log \
  "passed" \
  "recovery_validation" \
  "fallback_metadata_present" \
  "recovery_path_validated" \
  "none" \
  "$(basename "${tmp_recovery}")" \
  "recovery evidence accepted with fallback metadata"

emit_log \
  "running" \
  "doc_wiring" \
  "policy_reference_check" \
  "none" \
  "none" \
  "$(basename "${POLICY_DOC}")" \
  "checking policy docs reference schema and validator tooling"

rg -q "asupersync-rch-evidence-schema.json" "${POLICY_DOC}" || {
  emit_log \
    "failed" \
    "doc_wiring" \
    "policy_reference_check" \
    "missing_schema_reference" \
    "doc_reference_missing" \
    "$(basename "${POLICY_DOC}")" \
    "policy doc missing schema reference"
  exit 1
}

rg -q "validate_asupersync_rch_execution_policy.sh" "${POLICY_DOC}" || {
  emit_log \
    "failed" \
    "doc_wiring" \
    "policy_reference_check" \
    "missing_validator_reference" \
    "doc_reference_missing" \
    "$(basename "${POLICY_DOC}")" \
    "policy doc missing validator reference"
  exit 1
}

emit_log \
  "passed" \
  "doc_wiring" \
  "policy_reference_check" \
  "doc_wiring_valid" \
  "none" \
  "$(basename "${POLICY_DOC}")" \
  "policy doc references validated"

emit_log \
  "passed" \
  "suite_complete" \
  "unit_classifier->integration_valid_evidence->failure_injection->recovery_validation->doc_wiring" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.4 policy validation passed"

echo "ft-e34d9.10.1.4 policy e2e validation passed. Log: ${LOG_FILE}"
