#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_kvs1e_proof_ledger_wrapper_policy"
CORRELATION_ID="ft-kvs1e-${RUN_ID}"
LOG_FILE="${LOG_DIR}/proof_ledger_policy_${RUN_ID}.jsonl"

VALIDATOR="${ROOT_DIR}/scripts/validate_asupersync_rch_execution_policy.sh"
POLICY_DOC="${ROOT_DIR}/docs/asupersync-rch-execution-policy.md"
SCHEMA_DOC="${ROOT_DIR}/docs/asupersync-rch-evidence-schema.json"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${ROOT_DIR}/tests/e2e/lib_rch_guards.sh"

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

fingerprint_text() {
  local text="$1"
  local digest

  if command -v shasum >/dev/null 2>&1; then
    digest="$(printf '%s' "${text}" | shasum -a 256 | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    digest="$(printf '%s' "${text}" | sha256sum | awk '{print $1}')"
  else
    emit_log \
      "failed" \
      "suite_init" \
      "preflight_sha256" \
      "sha256_missing" \
      "sha256_not_found" \
      "$(basename "${LOG_FILE}")" \
      "shasum or sha256sum is required"
    exit 1
  fi

  printf 'sha256:%s' "${digest}"
}

artifact_paths_fingerprint() {
  local artifact_path="$1"
  fingerprint_text "$(jq -cn --arg path "${artifact_path}" '[$path]')"
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-kvs1e wrapper-emitted proof-ledger policy validation"

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

tmp_valid="${tmp_dir}/valid.json"
tmp_invalid="${tmp_dir}/invalid.json"
tmp_recovery="${tmp_dir}/recovery.json"
tmp_sync_chatter="${tmp_dir}/sync-chatter.json"
tmp_shell_wrapper="${tmp_dir}/shell-wrapper.json"
tmp_missing_artifact="${tmp_dir}/missing-artifact.json"
tmp_missing_is_heavy="${tmp_dir}/missing-is-heavy.json"
tmp_secret_command="${tmp_dir}/secret-command.json"
tmp_secret_path="${tmp_dir}/secret-path.json"
tmp_fallback_record="${tmp_dir}/fallback-required.json"
tmp_timeout_record="${tmp_dir}/timeout.json"
tmp_malformed_bead="${tmp_dir}/malformed-bead.json"
tmp_stale_schema="${tmp_dir}/stale-schema.json"

secret_fixture="API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz012345 cargo test -p frankenterm-core --header 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345' --path /Users/jemanuel/.ssh/id_ed25519 --safe crates/frankenterm"
redaction_json="$("${VALIDATOR}" --redact-text "${secret_fixture}")"
redacted_summary="$(jq -r '.redacted' <<<"${redaction_json}")"
redaction_fingerprint="$(jq -r '.fingerprint' <<<"${redaction_json}")"
if [[ "${redacted_summary}" == *"sk-proj-"* || "${redacted_summary}" == *"Bearer abcdef"* || "${redacted_summary}" == *"/Users/jemanuel"* ]]; then
  emit_log \
    "failed" \
    "redaction_helper" \
    "redact_text" \
    "secret_leaked" \
    "redaction_failed" \
    "$(basename "${VALIDATOR}")" \
    "redaction helper leaked fixture"
  exit 1
fi
if [[ "${redacted_summary}" != *"cargo test -p frankenterm-core"* || "${redacted_summary}" != *"crates/frankenterm"* ]]; then
  emit_log \
    "failed" \
    "redaction_helper" \
    "redact_text" \
    "structure_lost" \
    "redaction_overapplied" \
    "$(basename "${VALIDATOR}")" \
    "redaction helper removed non-sensitive structure"
  exit 1
fi
if [[ ! "${redaction_fingerprint}" =~ ^sha256:[a-f0-9]{64}$ ]]; then
  emit_log \
    "failed" \
    "redaction_helper" \
    "fingerprint_shape" \
    "bad_fingerprint" \
    "invalid_fingerprint" \
    "$(basename "${VALIDATOR}")" \
    "redaction helper emitted invalid fingerprint"
  exit 1
fi
emit_log \
  "passed" \
  "redaction_helper" \
  "redact_text" \
  "secret_redacted_structure_preserved" \
  "none" \
  "$(basename "${VALIDATOR}")" \
  "${redacted_summary}"

_RCH_REPO_ROOT="${ROOT_DIR}"
wrapper_ledger="${tmp_dir}/wrapper-ledger.jsonl"
mock_rch_log="${tmp_dir}/wrapper-rch.log"
cat >"${mock_rch_log}" <<'LOG'
Selected worker: contabo-2 at 10.0.0.12
Sync complete: workspace in 42ms
Remote command finished: exit=0 in 11100ms
LOG
rch_write_meta_json "${mock_rch_log}" "0"
RCH_PROOF_LEDGER_FILE="${wrapper_ledger}" \
RCH_PROOF_LEDGER_BEAD_ID="ft-kvs1e" \
RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}" \
  rch_emit_proof_ledger_entry \
    "run_rch_cargo_logged ${mock_rch_log} env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace" \
    "${mock_rch_log}" \
    "0" \
    "target/rch-proof" \
    "retained" \
    ""
sed -n '1p' "${wrapper_ledger}" >"${tmp_valid}"

while IFS= read -r artifact_path; do
  if [[ ! -e "${ROOT_DIR}/${artifact_path}" && ! -e "${artifact_path}" ]]; then
    emit_log \
      "failed" \
      "wrapper_ledger" \
      "artifact_retention" \
      "missing_artifact" \
      "artifact_not_retained" \
      "${artifact_path}" \
      "wrapper-emitted artifact path must exist"
    exit 1
  fi
done < <(jq -r '.runs[0].artifact_paths[]' "${tmp_valid}")

fallback_log="${tmp_dir}/fallback.log"
printf '%s\n' "[RCH] local fallback running locally" >"${fallback_log}"
rch_write_meta_json "${fallback_log}" "0"
RCH_PROOF_LEDGER_FILE="${wrapper_ledger}" \
RCH_PROOF_LEDGER_BEAD_ID="ft-kvs1e" \
RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}" \
  rch_emit_proof_ledger_entry \
    "run_rch_cargo_logged ${fallback_log} env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace" \
    "${fallback_log}" \
    "0" \
    "target/rch-proof" \
    "retained" \
    "local fallback marker detected"
tail -n 1 "${wrapper_ledger}" >"${tmp_fallback_record}"
if [[ "$(jq -r '.runs[0].validation_status' "${tmp_fallback_record}")" != "fallback_required" ]]; then
  emit_log \
    "failed" \
    "wrapper_ledger" \
    "local_fallback_detection" \
    "fallback_not_marked" \
    "unexpected_validation_status" \
    "$(basename "${tmp_fallback_record}")" \
    "local fallback record must be marked fallback_required"
  exit 1
fi
expect_validation_failure \
  "${tmp_fallback_record}" \
  "wrapper_ledger" \
  "local_fallback_detection" \
  "wrapper-emitted local fallback record must not validate as passing proof"

timeout_log="${tmp_dir}/timeout.log"
printf '%s\n' "Remote command still running" >"${timeout_log}"
rch_write_meta_json "${timeout_log}" "124"
RCH_PROOF_LEDGER_FILE="${wrapper_ledger}" \
RCH_PROOF_LEDGER_BEAD_ID="ft-kvs1e" \
RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}" \
  rch_emit_proof_ledger_entry \
    "run_rch_cargo_logged_with_timeout 1 ${timeout_log} env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace" \
    "${timeout_log}" \
    "124" \
    "target/rch-proof" \
    "retained" \
    "timeout fixture"
tail -n 1 "${wrapper_ledger}" >"${tmp_timeout_record}"
if [[ "$(jq -r '.runs[0].validation_status' "${tmp_timeout_record}")" != "timeout" ]]; then
  emit_log \
    "failed" \
    "wrapper_ledger" \
    "timeout_classification" \
    "timeout_not_marked" \
    "unexpected_validation_status" \
    "$(basename "${tmp_timeout_record}")" \
    "timeout record must be marked timeout"
  exit 1
fi
expect_validation_failure \
  "${tmp_timeout_record}" \
  "wrapper_ledger" \
  "timeout_classification" \
  "wrapper-emitted timeout record must not validate as passing proof"

if (
  RCH_PROOF_LEDGER_FILE="${tmp_dir}/missing-metadata.jsonl" \
  RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}" \
    rch_emit_proof_ledger_entry \
      "run_rch_cargo_logged ${mock_rch_log} env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace" \
      "${mock_rch_log}" \
      "0" \
      "target/rch-proof" \
      "retained" \
      ""
) >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "wrapper_ledger" \
    "missing_bead_metadata" \
    "missing_metadata_allowed" \
    "guardrail_not_enforced" \
    "missing-metadata.jsonl" \
    "proof-ledger emission without bead metadata must fail"
  exit 1
fi

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

jq --arg cmd "cargo test --workspace" \
  --arg cmd_fp "$(fingerprint_text "cargo test --workspace")" \
  --arg worker "local" \
  --arg worker_fp "$(fingerprint_text "local")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].used_rch = false |
    .runs[0].worker_context = $worker |
    .runs[0].worker_context_fingerprint = $worker_fp |
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

jq --arg cmd "rch status && cargo test --workspace" \
  --arg cmd_fp "$(fingerprint_text "rch status && cargo test --workspace")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].used_rch = true |
    .runs[0].execution_mode = "remote_rch"' \
  "${tmp_valid}" > "${tmp_sync_chatter}"
expect_validation_failure \
  "${tmp_sync_chatter}" \
  "failure_injection" \
  "sync_chatter_false_proof" \
  "RCH status/setup chatter must not count as remote Cargo proof"

jq --arg cmd "bash -lc 'echo rch exec -- cargo test; cargo test --workspace'" \
  --arg cmd_fp "$(fingerprint_text "bash -lc 'echo rch exec -- cargo test; cargo test --workspace'")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].used_rch = true |
    .runs[0].execution_mode = "remote_rch"' \
  "${tmp_valid}" > "${tmp_shell_wrapper}"
expect_validation_failure \
  "${tmp_shell_wrapper}" \
  "failure_injection" \
  "shell_wrapper_false_proof" \
  "shell wrapper that only mentions RCH must not validate as RCH proof"

missing_artifact_rel="${mock_rch_log#"${ROOT_DIR}"/}"
missing_artifact_rel="${missing_artifact_rel%/*}/missing.jsonl"
jq --arg missing "${missing_artifact_rel}" \
  --arg artifact_fp "$(artifact_paths_fingerprint "${missing_artifact_rel}")" \
  '.runs[0].artifact_paths = [$missing] |
    .runs[0].artifact_paths_fingerprint = $artifact_fp' \
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

jq --arg cmd "API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz rch exec -- cargo test --workspace" \
  --arg cmd_fp "$(fingerprint_text "API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz rch exec -- cargo test --workspace")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp' \
  "${tmp_valid}" > "${tmp_secret_command}"
emit_log \
  "running" \
  "failure_injection" \
  "unredacted_command_secret" \
  "none" \
  "none" \
  "$(basename "${tmp_secret_command}")" \
  "unredacted secret-bearing command must fail validation"
secret_error="$("${VALIDATOR}" --validate-evidence "${tmp_secret_command}" 2>&1 >/dev/null || true)"
if [[ -z "${secret_error}" || "${secret_error}" == *"sk-proj-"* ]]; then
  emit_log \
    "failed" \
    "failure_injection" \
    "unredacted_command_secret" \
    "secret_error_leaked_or_missing" \
    "redaction_error_contract_failed" \
    "$(basename "${tmp_secret_command}")" \
    "validator error failed the no-secret-leak contract"
  exit 1
fi
emit_log \
  "passed" \
  "failure_injection" \
  "unredacted_command_secret" \
  "negative_guardrail_enforced" \
  "none" \
  "$(basename "${tmp_secret_command}")" \
  "validator rejected secret-bearing command without echoing the raw secret"

jq --arg path "${tmp_dir}/.ssh/id_ed25519" \
  --arg path_fp "$(fingerprint_text "${tmp_dir}/.ssh/id_ed25519")" \
  '.runs[0].target_dir = $path |
    .runs[0].target_dir_fingerprint = $path_fp' \
  "${tmp_valid}" > "${tmp_secret_path}"
expect_validation_failure \
  "${tmp_secret_path}" \
  "failure_injection" \
  "ssh_secret_path" \
  "SSH-style secret paths must fail validation"

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

if rg -q "sk-proj-|Bearer abcdef|/Users/jemanuel" "${LOG_FILE}"; then
  emit_log \
    "failed" \
    "redaction_helper" \
    "aggregate_log_scan" \
    "secret_leaked" \
    "aggregate_output_leak" \
    "$(basename "${LOG_FILE}")" \
    "aggregate E2E log contains a raw fixture secret"
  exit 1
fi

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
  "ft-kvs1e proof-ledger wrapper policy validation passed"

echo "ft-kvs1e proof-ledger wrapper policy e2e validation passed. Log: ${LOG_FILE}"
