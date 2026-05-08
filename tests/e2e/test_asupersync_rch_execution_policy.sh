#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_emjsg_proof_ledger_policy"
CORRELATION_ID="ft-emjsg-${RUN_ID}"
LOG_FILE="${LOG_DIR}/proof_ledger_policy_${RUN_ID}.jsonl"

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

expect_validation_failure() {
  local evidence_file="$1"
  local scenario="$2"
  local decision_path="$3"
  local input_summary="$4"

  emit_log \
    "running" \
    "${scenario}" \
    "${decision_path}" \
    "none" \
    "none" \
    "$(basename "${evidence_file}")" \
    "${input_summary}"

  if "${VALIDATOR}" --validate-evidence "${evidence_file}" >/dev/null 2>&1; then
    emit_log \
      "failed" \
      "${scenario}" \
      "${decision_path}" \
      "guardrail_not_enforced" \
      "unexpected_negative_pass" \
      "$(basename "${evidence_file}")" \
      "invalid evidence unexpectedly passed"
    exit 1
  fi

  emit_log \
    "passed" \
    "${scenario}" \
    "${decision_path}" \
    "negative_guardrail_enforced" \
    "none" \
    "$(basename "${evidence_file}")" \
    "invalid evidence correctly rejected"
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-emjsg repository proof-ledger policy validation"

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

install_no_rch="$("${VALIDATOR}" --classify "cargo install --locked --path crates/frankenterm")"
if [[ "$(jq -r '.is_heavy' <<<"${install_no_rch}")" != "true" || "$(jq -r '.policy_violation' <<<"${install_no_rch}")" != "true" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "cargo install should be heavy and policy violation without rch"
  exit 1
fi

wrapped_rch="$("${VALIDATOR}" --classify "run_rch_cargo_logged target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace")"
if [[ "$(jq -r '.is_heavy' <<<"${wrapped_rch}")" != "true" || "$(jq -r '.used_rch' <<<"${wrapped_rch}")" != "true" || "$(jq -r '.policy_violation' <<<"${wrapped_rch}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "run_rch_cargo_logged should be heavy, rch-backed, and policy-compliant"
  exit 1
fi

wrapped_install_rch="$("${VALIDATOR}" --classify "run_rch_cargo_logged target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo install --locked --path crates/frankenterm")"
if [[ "$(jq -r '.is_heavy' <<<"${wrapped_install_rch}")" != "true" || "$(jq -r '.used_rch' <<<"${wrapped_install_rch}")" != "true" || "$(jq -r '.policy_violation' <<<"${wrapped_install_rch}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "run_rch_cargo_logged cargo install should be heavy, rch-backed, and policy-compliant"
  exit 1
fi

timeout_wrapped_rch="$("${VALIDATOR}" --classify "run_rch_cargo_logged_with_timeout 120 target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace")"
if [[ "$(jq -r '.is_heavy' <<<"${timeout_wrapped_rch}")" != "true" || "$(jq -r '.used_rch' <<<"${timeout_wrapped_rch}")" != "true" || "$(jq -r '.policy_violation' <<<"${timeout_wrapped_rch}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "run_rch_cargo_logged_with_timeout should be heavy, rch-backed, and policy-compliant"
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

tmp_dir="${LOG_DIR}/asupersync_rch_policy_${RUN_ID}_evidence"
mkdir -p "${tmp_dir}"
mock_artifact="${tmp_dir}/mock_rch_policy.jsonl"
printf '{"mock":true}\n' > "${mock_artifact}"

tmp_valid="${tmp_dir}/valid.json"
tmp_invalid="${tmp_dir}/invalid.json"
tmp_recovery="${tmp_dir}/recovery.json"
tmp_sync_chatter="${tmp_dir}/sync-chatter.json"
tmp_shell_wrapper="${tmp_dir}/shell-wrapper.json"
tmp_missing_artifact="${tmp_dir}/missing-artifact.json"
tmp_missing_is_heavy="${tmp_dir}/missing-is-heavy.json"
tmp_malformed_bead="${tmp_dir}/malformed-bead.json"
tmp_stale_schema="${tmp_dir}/stale-schema.json"

cat > "${tmp_valid}" <<JSON
{
  "schema_version": 2,
  "bead_id": "ft-emjsg",
  "policy_version": "2.0.0",
  "runs": [
    {
      "timestamp": "2026-02-25T00:00:00Z",
      "command": "rch exec -- cargo check --workspace --all-targets",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=contabo-2",
      "execution_mode": "remote_rch",
      "target_dir": "/tmp/ft-emjsg-rch-target",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["${mock_artifact}"],
      "elapsed_seconds": 31.4,
      "exit_status": 0,
      "residual_risk_notes": "",
      "validation_status": "valid"
    },
    {
      "timestamp": "2026-02-25T00:01:00Z",
      "command": "run_rch_cargo_logged target/proof.log env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace",
      "command_class": "heavy",
      "is_heavy": true,
      "used_rch": true,
      "worker_context": "worker=contabo-2",
      "execution_mode": "remote_rch",
      "target_dir": "target/rch-proof",
      "target_dir_lifecycle": "retained",
      "artifact_paths": ["${mock_artifact}"],
      "elapsed_seconds": 11.1,
      "exit_status": 0,
      "residual_risk_notes": "",
      "validation_status": "valid"
    },
    {
      "timestamp": "2026-02-25T00:02:00Z",
      "command": "cargo fmt --check",
      "command_class": "light",
      "is_heavy": false,
      "used_rch": false,
      "worker_context": "local",
      "execution_mode": "local_light",
      "target_dir": "not_applicable",
      "target_dir_lifecycle": "not_applicable",
      "artifact_paths": ["${mock_artifact}"],
      "elapsed_seconds": 0.6,
      "exit_status": 0,
      "residual_risk_notes": "",
      "validation_status": "valid"
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

jq '.runs[0].command = "cargo test --workspace" |
    .runs[0].used_rch = false |
    .runs[0].worker_context = "local" |
    .runs[0].execution_mode = "remote_rch"' \
  "${tmp_valid}" > "${tmp_invalid}"

expect_validation_failure \
  "${tmp_invalid}" \
  "failure_injection" \
  "heavy_without_rch" \
  "heavy local run without fallback metadata should fail"

jq '.runs[0].fallback_reason_code = "RCH-E100" |
    .runs[0].fallback_approved_by = "human-operator" |
    .runs[0].execution_mode = "approved_local_fallback" |
    .runs[0].target_dir_lifecycle = "inventory_only" |
    .runs[0].validation_status = "approved_fallback"' \
  "${tmp_invalid}" > "${tmp_recovery}"

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

jq '.runs[0].command = "rch status && cargo test --workspace" |
    .runs[0].used_rch = true |
    .runs[0].execution_mode = "remote_rch"' \
  "${tmp_valid}" > "${tmp_sync_chatter}"
expect_validation_failure \
  "${tmp_sync_chatter}" \
  "failure_injection" \
  "sync_chatter_false_proof" \
  "RCH status/setup chatter must not count as remote Cargo proof"

jq '.runs[0].command = "bash -lc '\''echo rch exec -- cargo test; cargo test --workspace'\''" |
    .runs[0].used_rch = true |
    .runs[0].execution_mode = "remote_rch"' \
  "${tmp_valid}" > "${tmp_shell_wrapper}"
expect_validation_failure \
  "${tmp_shell_wrapper}" \
  "failure_injection" \
  "shell_wrapper_false_proof" \
  "shell wrapper that only mentions RCH must not validate as RCH proof"

jq --arg missing "${tmp_dir}/missing.jsonl" '.runs[0].artifact_paths = [$missing]' \
  "${tmp_valid}" > "${tmp_missing_artifact}"
expect_validation_failure \
  "${tmp_missing_artifact}" \
  "failure_injection" \
  "missing_artifact_path" \
  "missing artifact paths must fail validation"

jq 'del(.runs[0].is_heavy)' "${tmp_valid}" > "${tmp_missing_is_heavy}"
expect_validation_failure \
  "${tmp_missing_is_heavy}" \
  "failure_injection" \
  "missing_is_heavy" \
  "missing is_heavy must fail validation"

jq '.bead_id = "wa-old.1"' "${tmp_valid}" > "${tmp_malformed_bead}"
expect_validation_failure \
  "${tmp_malformed_bead}" \
  "failure_injection" \
  "malformed_bead_id" \
  "non-ft or malformed bead IDs must fail validation"

jq '.schema_version = 1' "${tmp_valid}" > "${tmp_stale_schema}"
expect_validation_failure \
  "${tmp_stale_schema}" \
  "failure_injection" \
  "stale_schema_version" \
  "stale schema versions must fail validation"

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
  "ft-emjsg proof-ledger policy validation passed"

echo "ft-emjsg proof-ledger policy e2e validation passed. Log: ${LOG_FILE}"
