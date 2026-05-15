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
VERIFICATION_DOC="${ROOT_DIR}/docs/ft-xbnl0-verification-contract.md"
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

for artifact in "${VALIDATOR}" "${POLICY_DOC}" "${VERIFICATION_DOC}" "${SCHEMA_DOC}"; do
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

dry_run_diagnose="$("${VALIDATOR}" --classify "rch --json diagnose --dry-run cargo check --help")"
if [[ "$(jq -r '.is_heavy' <<<"${dry_run_diagnose}")" != "false" || "$(jq -r '.policy_violation' <<<"${dry_run_diagnose}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "rch diagnose --dry-run cargo preflight should be light inventory, not a heavy cargo run"
  exit 1
fi

dry_run_words_after_exec="$("${VALIDATOR}" --classify "rch exec -- cargo test diagnose --dry-run")"
if [[ "$(jq -r '.is_heavy' <<<"${dry_run_words_after_exec}")" != "true" || "$(jq -r '.used_rch' <<<"${dry_run_words_after_exec}")" != "true" || "$(jq -r '.policy_violation' <<<"${dry_run_words_after_exec}")" != "false" ]]; then
  emit_log \
    "failed" \
    "unit_classifier" \
    "command_classification" \
    "classifier_mismatch" \
    "unexpected_classifier_result" \
    "$(basename "${VALIDATOR}")" \
    "dry-run words after rch exec must not hide a real cargo test"
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
tmp_missing_worker_id="${tmp_dir}/missing-worker-id.json"
tmp_target_worker_mismatch="${tmp_dir}/target-worker-mismatch.json"
tmp_target_worker_mismatch_ledger="${tmp_dir}/target-worker-mismatch-ledger.jsonl"
tmp_target_worker_mismatch_report="${tmp_dir}/target-worker-mismatch-report.json"
tmp_missing_repo_snapshot="${tmp_dir}/missing-repo-snapshot.json"
tmp_bad_target_mirror_status="${tmp_dir}/bad-target-mirror-status.json"
tmp_mirror_failed_attestation="${tmp_dir}/mirror-failed-attestation.json"
tmp_mirror_failed_ledger="${tmp_dir}/mirror-failed-ledger.jsonl"
tmp_mirror_failed_report="${tmp_dir}/mirror-failed-ledger-report.json"
tmp_worker_self_test="${tmp_dir}/worker-self-test-only.json"
tmp_worker_self_test_ledger="${tmp_dir}/worker-self-test-only-ledger.jsonl"
tmp_worker_self_test_report="${tmp_dir}/worker-self-test-only-report.json"
tmp_light_local="${tmp_dir}/light-local.json"
tmp_residual_risk="${tmp_dir}/residual-risk.json"
tmp_mixed_ledger_a="${tmp_dir}/mixed-ledger-a.jsonl"
tmp_mixed_ledger_b="${tmp_dir}/mixed-ledger-b.jsonl"
tmp_mixed_report="${tmp_dir}/mixed-ledger-report.json"
tmp_rejected_ledger="${tmp_dir}/rejected-ledger.jsonl"
tmp_rejected_report="${tmp_dir}/rejected-ledger-report.json"
tmp_missing_artifact_ledger="${tmp_dir}/missing-artifact-ledger.jsonl"
tmp_missing_artifact_report="${tmp_dir}/missing-artifact-ledger-report.json"
tmp_malformed_ledger="${tmp_dir}/malformed-ledger.jsonl"
tmp_malformed_report="${tmp_dir}/malformed-ledger-report.json"

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

if [[ "$(jq -r '.runs[0].worker_evidence_confidence' "${tmp_valid}")" != "scheduler_selected_remote_proof" ]] \
  || [[ "$(jq -r '.runs[0].selected_worker_id' "${tmp_valid}")" != "contabo-2" ]] \
  || [[ "$(jq -r '.runs[0].source_mirror_status' "${tmp_valid}")" != "present" ]] \
  || [[ "$(jq -r '.runs[0].remote_cargo_reached' "${tmp_valid}")" != "true" ]]; then
  emit_log \
    "failed" \
    "wrapper_ledger" \
    "worker_evidence_fields" \
    "worker_evidence_missing" \
    "unexpected_worker_evidence" \
    "$(basename "${tmp_valid}")" \
    "wrapper-emitted remote proof must carry scheduler-selected worker and source-mirror fields"
  exit 1
fi

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

strict_rch_dir="${tmp_dir}/strict-remote-bin"
strict_rch_env_log="${tmp_dir}/strict-remote-env.log"
strict_rch_output_log="${tmp_dir}/strict-remote-wrapper.log"
strict_rch_error_log="${tmp_dir}/strict-remote-wrapper.stderr.log"
mkdir -p "${strict_rch_dir}"
cat >"${strict_rch_dir}/rch" <<'FAKE_RCH'
#!/usr/bin/env bash
set -euo pipefail
: "${STRICT_RCH_ENV_LOG:?}"
{
  printf 'RCH_REQUIRE_REMOTE=%s\n' "${RCH_REQUIRE_REMOTE:-}"
  printf 'RCH_BUILD_SLOTS=%s\n' "${RCH_BUILD_SLOTS:-}"
  printf 'RCH_TEST_SLOTS=%s\n' "${RCH_TEST_SLOTS:-}"
  printf 'RCH_CHECK_SLOTS=%s\n' "${RCH_CHECK_SLOTS:-}"
  printf 'args=%s\n' "$*"
} >>"${STRICT_RCH_ENV_LOG}"

if [[ "${1:-}" == "exec" ]]; then
  printf '%s\n' "[RCH] local (selection error: queue_timeout)"
  printf '%s\n' "[RCH] remote required; refusing local fallback (no worker assigned)"
  exit 1
fi

printf 'unexpected fake rch invocation: %s\n' "$*" >&2
exit 64
FAKE_RCH
chmod +x "${strict_rch_dir}/rch"

set +e
(
  PATH="${strict_rch_dir}:${PATH}" \
    STRICT_RCH_ENV_LOG="${strict_rch_env_log}" \
    RCH_BUILD_SLOTS=8 \
    RCH_TEST_SLOTS=8 \
    RCH_CHECK_SLOTS=2 \
    run_rch_cargo_logged \
      "${strict_rch_output_log}" \
      env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace
) >/dev/null 2>"${strict_rch_error_log}"
strict_rch_rc=$?
set -e

if [[ "${strict_rch_rc}" -eq 0 ]] \
  || ! grep -Fxq "RCH_REQUIRE_REMOTE=1" "${strict_rch_env_log}" \
  || ! grep -Fxq "RCH_BUILD_SLOTS=8" "${strict_rch_env_log}" \
  || ! grep -Fxq "RCH_TEST_SLOTS=8" "${strict_rch_env_log}" \
  || ! grep -Fxq "RCH_CHECK_SLOTS=2" "${strict_rch_env_log}" \
  || ! grep -Fq "remote required; refusing local fallback" "${strict_rch_output_log}" \
  || ! grep -Fq "refusing offload policy violation" "${strict_rch_error_log}"; then
  emit_log \
    "failed" \
    "wrapper_guard" \
    "strict_remote_env" \
    "strict_remote_not_enforced" \
    "wrapper_failed_open" \
    "$(basename "${strict_rch_output_log}")" \
    "run_rch_cargo_logged must pass RCH_REQUIRE_REMOTE=1 plus slot envs and fail before local cargo fallback"
  exit 1
fi

emit_log \
  "passed" \
  "wrapper_guard" \
  "strict_remote_env" \
  "strict_remote_enforced" \
  "none" \
  "$(basename "${strict_rch_output_log}")" \
  "run_rch_cargo_logged passes RCH_REQUIRE_REMOTE=1 plus slot envs and fails closed on fake queue_timeout fallback"

nonzero_rch_dir="${tmp_dir}/nonzero-remote-bin"
nonzero_rch_output_log="${tmp_dir}/nonzero-remote-wrapper.log"
nonzero_rch_error_log="${tmp_dir}/nonzero-remote-wrapper.stderr.log"
nonzero_rch_marker="${tmp_dir}/nonzero-remote-after.marker"
mkdir -p "${nonzero_rch_dir}"
cat >"${nonzero_rch_dir}/rch" <<'FAKE_RCH'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "exec" ]]; then
  printf '%s\n' "Selected worker: fake-worker at ubuntu@127.0.0.1 (0 slots, speed 1.0)"
  printf '%s\n' "Remote command finished: exit=101 in 42ms"
  exit 101
fi

printf 'unexpected fake rch invocation: %s\n' "$*" >&2
exit 64
FAKE_RCH
chmod +x "${nonzero_rch_dir}/rch"

set +e
(
  PATH="${nonzero_rch_dir}:${PATH}" \
    RCH_PROOF_LEDGER_FILE="${wrapper_ledger}" \
    RCH_PROOF_LEDGER_BEAD_ID="ft-kvs1e" \
    RCH_PROOF_LEDGER_SCENARIO_ID="${SCENARIO_ID}" \
    run_rch_cargo_logged \
      "${nonzero_rch_output_log}" \
      env CARGO_TARGET_DIR=target/rch-proof cargo test --workspace
  nonzero_inner_rc=$?
  printf 'after:%s\n' "${nonzero_inner_rc}" >"${nonzero_rch_marker}"
  exit "${nonzero_inner_rc}"
) >/dev/null 2>"${nonzero_rch_error_log}"
nonzero_rch_rc=$?
set -e

if [[ "${nonzero_rch_rc}" -ne 101 ]] \
  || ! grep -Fxq "after:101" "${nonzero_rch_marker}" \
  || ! grep -Fq "Remote command finished: exit=101" "${nonzero_rch_output_log}"; then
  emit_log \
    "failed" \
    "wrapper_guard" \
    "nonzero_capture" \
    "errexit_state_leaked" \
    "post_failure_block_not_reached" \
    "$(basename "${nonzero_rch_output_log}")" \
    "run_rch_cargo_logged must preserve set +e callers so harnesses can record failure JSON"
  exit 1
fi

emit_log \
  "passed" \
  "wrapper_guard" \
  "nonzero_capture" \
  "errexit_state_preserved" \
  "none" \
  "$(basename "${nonzero_rch_output_log}")" \
  "run_rch_cargo_logged preserves set +e callers after non-fallback remote failures"

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
    .runs[0].validation_status = "approved_fallback" |
    .runs[0].worker_evidence_confidence = "inconclusive_worker_evidence" |
    .runs[0].selected_worker_id = null |
    .runs[0].worker_queue_state = "unsupported_worker_selection" |
    .runs[0].source_mirror_status = "not_checked" |
    .runs[0].remote_cargo_reached = false |
    .runs[0].remote_rustc_reached = false |
    .runs[0].test_binary_reached = false' \
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

jq '.runs[0].worker_evidence_confidence = "target_worker_remote_proof" |
    .runs[0].intended_worker_id = "contabo-2" |
    .runs[0].selected_worker_id = null' \
  "${tmp_valid}" > "${tmp_missing_worker_id}"
expect_validation_failure \
  "${tmp_missing_worker_id}" \
  "failure_injection" \
  "target_worker_missing_selected_worker" \
  "target-worker proof without selected worker id must fail validation"

jq '.runs[0].worker_evidence_confidence = "target_worker_remote_proof" |
    .runs[0].intended_worker_id = "contabo-2" |
    .runs[0].selected_worker_id = "contabo-3" |
    .runs[0].worker_context = "worker=contabo-3" |
    .runs[0].worker_context_fingerprint = "'"$(fingerprint_text "worker=contabo-3")"'"' \
  "${tmp_valid}" > "${tmp_target_worker_mismatch}"
expect_validation_failure \
  "${tmp_target_worker_mismatch}" \
  "failure_injection" \
  "target_worker_selected_mismatch" \
  "target-worker proof must reject scheduler-selected worker mismatch"

jq -c . "${tmp_target_worker_mismatch}" > "${tmp_target_worker_mismatch_ledger}"
set +e
"${VALIDATOR}" --aggregate-ledger "${tmp_target_worker_mismatch_ledger}" > "${tmp_target_worker_mismatch_report}" 2>/dev/null
aggregate_target_mismatch_rc=$?
set -e
if [[ "${aggregate_target_mismatch_rc}" -eq 0 ]]; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "target_worker_selected_mismatch" \
    "negative_guardrail_not_enforced" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_target_worker_mismatch_report}")" \
    "aggregate gate must reject target-worker evidence when the selected worker differs"
  exit 1
fi

jq -e \
  --arg scenario "${SCENARIO_ID}" \
  '.quality_gate_passed == false and
   .blocking_failure_count == 1 and
   .entries[0].bead_id == "ft-kvs1e" and
   .entries[0].scenario_id == $scenario and
   .entries[0].worker_context == "worker=contabo-3" and
   .entries[0].artifact_path != "unknown" and
   (.entries[0].reason_detail | contains("target_worker_remote_proof requires intended_worker_id to match selected_worker_id"))' \
  "${tmp_target_worker_mismatch_report}" >/dev/null || {
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "target_worker_selected_mismatch" \
    "missing_operator_fields" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_target_worker_mismatch_report}")" \
    "rejected target-worker aggregate entry must name bead, scenario, worker, artifact, and mismatch reason"
  exit 1
}

jq '.runs[0].worker_evidence_confidence = "scheduler_selected_remote_proof" |
    .runs[0].repo_snapshot_head = null' \
  "${tmp_valid}" > "${tmp_missing_repo_snapshot}"
expect_validation_failure \
  "${tmp_missing_repo_snapshot}" \
  "failure_injection" \
  "scheduler_selected_missing_repo_snapshot" \
  "scheduler-selected proof without source snapshot must fail validation"

jq '.runs[0].worker_evidence_confidence = "target_worker_remote_proof" |
    .runs[0].intended_worker_id = "contabo-2" |
    .runs[0].selected_worker_id = "contabo-2" |
    .runs[0].source_mirror_status = "missing" |
    .runs[0].source_mirror_reason_code = "rch_mirror.missing_tracked_file"' \
  "${tmp_valid}" > "${tmp_bad_target_mirror_status}"
expect_validation_failure \
  "${tmp_bad_target_mirror_status}" \
  "failure_injection" \
  "target_worker_missing_source_mirror" \
  "target-worker remote proof with missing source mirror must fail validation"

mock_rch_log_rel="${mock_rch_log#"${ROOT_DIR}"/}"
mirror_cmd="bash scripts/attest_rch_worker_mirror.sh --worker contabo-2 --workspace-member-roots --path Cargo.toml --json > ${mock_rch_log_rel}"
mirror_worker="worker=contabo-2"
mirror_residual="mirror attestation showed the named worker is missing a tracked file; no material Cargo proof ran"
jq --arg cmd "${mirror_cmd}" \
  --arg cmd_fp "$(fingerprint_text "${mirror_cmd}")" \
  --arg worker "${mirror_worker}" \
  --arg worker_fp "$(fingerprint_text "${mirror_worker}")" \
  --arg target "not_applicable" \
  --arg target_fp "$(fingerprint_text "not_applicable")" \
  --arg residual "${mirror_residual}" \
  --arg residual_fp "$(fingerprint_text "${mirror_residual}")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].command_class = "light" |
    .runs[0].is_heavy = false |
    .runs[0].used_rch = false |
    .runs[0].worker_context = $worker |
    .runs[0].worker_context_fingerprint = $worker_fp |
    .runs[0].execution_mode = "local_light" |
    .runs[0].target_dir = $target |
    .runs[0].target_dir_fingerprint = $target_fp |
    .runs[0].target_dir_lifecycle = "not_applicable" |
    .runs[0].residual_risk_notes = $residual |
    .runs[0].residual_risk_notes_fingerprint = $residual_fp |
    .runs[0].worker_evidence_confidence = "target_worker_mirror_attestation" |
    .runs[0].intended_worker_id = "contabo-2" |
    .runs[0].selected_worker_id = "contabo-2" |
    .runs[0].worker_queue_state = "not_applicable" |
    .runs[0].repo_snapshot_head = "unknown" |
    .runs[0].source_mirror_status = "missing" |
    .runs[0].source_mirror_reason_code = "rch_mirror.missing_tracked_file" |
    .runs[0].remote_cargo_reached = false |
    .runs[0].remote_rustc_reached = false |
    .runs[0].test_binary_reached = false' \
  "${tmp_valid}" > "${tmp_mirror_failed_attestation}"

if ! "${VALIDATOR}" --validate-evidence "${tmp_mirror_failed_attestation}" >/dev/null; then
  emit_log \
    "failed" \
    "worker_evidence" \
    "mirror_failed_attestation" \
    "unexpected_mirror_attestation_reject" \
    "validator_rejected_mirror_failed_attestation" \
    "$(basename "${tmp_mirror_failed_attestation}")" \
    "mirror-failed static attestation should validate as diagnostic evidence"
  exit 1
fi

jq -c . "${tmp_mirror_failed_attestation}" > "${tmp_mirror_failed_ledger}"
if ! "${VALIDATOR}" --aggregate-ledger "${tmp_mirror_failed_ledger}" > "${tmp_mirror_failed_report}"; then
  emit_log \
    "failed" \
    "worker_evidence" \
    "mirror_failed_aggregate" \
    "aggregate_unexpected_fail" \
    "aggregate_report_failed" \
    "$(basename "${tmp_mirror_failed_report}")" \
    "mirror-failed static attestation aggregate should pass with partial risk"
  exit 1
fi

jq -e '
  .overall_verdict == "partial_risk" and
  .quality_gate_passed == true and
  .counts.residual_risk_only == 1 and
  .worker_evidence_counts.mirror_failed == 1 and
  .entries[0].worker_evidence_confidence == "target_worker_mirror_attestation" and
  .entries[0].source_mirror_status == "missing"
' "${tmp_mirror_failed_report}" >/dev/null || {
  emit_log \
    "failed" \
    "worker_evidence" \
    "mirror_failed_aggregate" \
    "missing_worker_fields" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_mirror_failed_report}")" \
    "mirror-failed aggregate must expose worker-evidence confidence and mirror status"
  exit 1
}

self_test_cmd="rch exec -- rch check"
self_test_worker="worker=contabo-2"
self_test_residual="worker self-test only; material proof command did not run"
jq --arg cmd "${self_test_cmd}" \
  --arg cmd_fp "$(fingerprint_text "${self_test_cmd}")" \
  --arg worker "${self_test_worker}" \
  --arg worker_fp "$(fingerprint_text "${self_test_worker}")" \
  --arg target "not_applicable" \
  --arg target_fp "$(fingerprint_text "not_applicable")" \
  --arg residual "${self_test_residual}" \
  --arg residual_fp "$(fingerprint_text "${self_test_residual}")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].command_class = "light" |
    .runs[0].is_heavy = false |
    .runs[0].used_rch = true |
    .runs[0].worker_context = $worker |
    .runs[0].worker_context_fingerprint = $worker_fp |
    .runs[0].execution_mode = "remote_rch" |
    .runs[0].target_dir = $target |
    .runs[0].target_dir_fingerprint = $target_fp |
    .runs[0].target_dir_lifecycle = "not_applicable" |
    .runs[0].residual_risk_notes = $residual |
    .runs[0].residual_risk_notes_fingerprint = $residual_fp |
    .runs[0].worker_evidence_confidence = "worker_self_test_only" |
    .runs[0].intended_worker_id = null |
    .runs[0].selected_worker_id = "contabo-2" |
    .runs[0].worker_queue_state = "ready" |
    .runs[0].repo_snapshot_head = "unknown" |
    .runs[0].source_mirror_status = "not_checked" |
    .runs[0].source_mirror_reason_code = null |
    .runs[0].remote_cargo_reached = false |
    .runs[0].remote_rustc_reached = false |
    .runs[0].test_binary_reached = false |
    .runs[0].validation_status = "valid"' \
  "${tmp_valid}" > "${tmp_worker_self_test}"

if ! "${VALIDATOR}" --validate-evidence "${tmp_worker_self_test}" >/dev/null; then
  emit_log \
    "failed" \
    "worker_evidence" \
    "worker_self_test_only" \
    "unexpected_self_test_reject" \
    "validator_rejected_self_test_only" \
    "$(basename "${tmp_worker_self_test}")" \
    "worker self-test evidence should validate only as diagnostic evidence"
  exit 1
fi

jq -c . "${tmp_worker_self_test}" > "${tmp_worker_self_test_ledger}"
if ! "${VALIDATOR}" --aggregate-ledger "${tmp_worker_self_test_ledger}" > "${tmp_worker_self_test_report}"; then
  emit_log \
    "failed" \
    "worker_evidence" \
    "worker_self_test_only_aggregate" \
    "aggregate_unexpected_fail" \
    "aggregate_report_failed" \
    "$(basename "${tmp_worker_self_test_report}")" \
    "worker self-test aggregate should pass as residual-risk-only evidence"
  exit 1
fi

jq -e '
  .overall_verdict == "partial_risk" and
  .quality_gate_passed == true and
  .counts.residual_risk_only == 1 and
  .worker_evidence_counts.worker_self_test_only == 1 and
  .entries[0].worker_evidence_confidence == "worker_self_test_only" and
  .entries[0].worker_evidence_category == "worker_self_test_only" and
  .entries[0].remote_cargo_reached == false and
  .entries[0].test_binary_reached == false
' "${tmp_worker_self_test_report}" >/dev/null || {
  emit_log \
    "failed" \
    "worker_evidence" \
    "worker_self_test_only_aggregate" \
    "missing_worker_fields" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_worker_self_test_report}")" \
    "worker self-test aggregate must remain diagnostic and never count as material proof"
  exit 1
}

emit_log \
  "running" \
  "aggregate_quality_gate" \
  "mixed_ledger_partial_risk" \
  "none" \
  "none" \
  "$(basename "${tmp_mixed_report}")" \
  "aggregate gate should classify remote, light local, approved fallback, and residual-risk-only records across multiple ledgers"

light_command="cargo fmt --check"
light_worker="local"
light_target="not_applicable"
light_empty_risk=""
jq --arg cmd "${light_command}" \
  --arg cmd_fp "$(fingerprint_text "${light_command}")" \
  --arg worker "${light_worker}" \
  --arg worker_fp "$(fingerprint_text "${light_worker}")" \
  --arg target "${light_target}" \
  --arg target_fp "$(fingerprint_text "${light_target}")" \
  --arg residual "${light_empty_risk}" \
  --arg residual_fp "$(fingerprint_text "${light_empty_risk}")" \
  '.runs[0].command = $cmd |
    .runs[0].command_fingerprint = $cmd_fp |
    .runs[0].command_class = "light" |
    .runs[0].is_heavy = false |
    .runs[0].used_rch = false |
    .runs[0].worker_context = $worker |
    .runs[0].worker_context_fingerprint = $worker_fp |
    .runs[0].execution_mode = "local_light" |
    .runs[0].target_dir = $target |
    .runs[0].target_dir_fingerprint = $target_fp |
    .runs[0].target_dir_lifecycle = "not_applicable" |
    .runs[0].residual_risk_notes = $residual |
    .runs[0].residual_risk_notes_fingerprint = $residual_fp |
    .runs[0].worker_evidence_confidence = "legacy_unknown_worker_evidence" |
    .runs[0].intended_worker_id = null |
    .runs[0].selected_worker_id = null |
    .runs[0].worker_queue_state = "not_applicable" |
    .runs[0].repo_snapshot_head = "not_applicable" |
    .runs[0].source_mirror_status = "not_applicable" |
    .runs[0].source_mirror_reason_code = null |
    .runs[0].remote_cargo_reached = false |
    .runs[0].remote_rustc_reached = false |
    .runs[0].test_binary_reached = false |
    .runs[0].validation_status = "valid"' \
  "${tmp_valid}" > "${tmp_light_local}"

residual_note="remote proof valid, but operator should cite the retained artifact bundle"
jq --arg residual "${residual_note}" \
  --arg residual_fp "$(fingerprint_text "${residual_note}")" \
  '.runs[0].residual_risk_notes = $residual |
    .runs[0].residual_risk_notes_fingerprint = $residual_fp' \
  "${tmp_valid}" > "${tmp_residual_risk}"

jq -c . "${tmp_valid}" "${tmp_light_local}" > "${tmp_mixed_ledger_a}"
jq -c . "${tmp_recovery}" "${tmp_residual_risk}" > "${tmp_mixed_ledger_b}"
if ! "${VALIDATOR}" --aggregate-ledger "${tmp_mixed_ledger_a}" "${tmp_mixed_ledger_b}" > "${tmp_mixed_report}"; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "mixed_ledger_partial_risk" \
    "aggregate_unexpected_fail" \
    "aggregate_report_failed" \
    "$(basename "${tmp_mixed_report}")" \
    "mixed valid ledger should not have blocking aggregate failures"
  exit 1
fi

if [[ "$(jq -r '.overall_verdict' "${tmp_mixed_report}")" != "partial_risk" ]]; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "mixed_ledger_partial_risk" \
    "wrong_verdict" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_mixed_report}")" \
    "mixed ledger must produce a partial_risk verdict"
  exit 1
fi

jq -e '
  .quality_gate_passed == true and
  .counts.proven_remote == 1 and
  .counts.light_local == 1 and
  .counts.approved_fallback == 1 and
  .counts.residual_risk_only == 1 and
  .worker_evidence_counts.scheduler_selected_remote == 2 and
  .worker_evidence_counts.inconclusive_worker_evidence == 1 and
  .worker_evidence_counts.legacy_unknown_worker_evidence == 1 and
  (.ledger_paths | length) == 2 and
  .blocking_failure_count == 0 and
  ([.entries[] | select(.bead_id == "ft-kvs1e" and .scenario_id != "unknown" and .command != "unknown" and .worker_context != "unknown" and .artifact_path != "unknown" and .reason_code != "" and .worker_evidence_confidence != "" and .worker_evidence_category != "")] | length) == 4
' "${tmp_mixed_report}" >/dev/null || {
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "mixed_ledger_partial_risk" \
    "missing_operator_fields" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_mixed_report}")" \
    "aggregate report must preserve bead, scenario, command, worker, artifact, and reason-code fields"
  exit 1
}

jq -c . "${tmp_invalid}" > "${tmp_rejected_ledger}"
set +e
"${VALIDATOR}" --aggregate-ledger "${tmp_rejected_ledger}" > "${tmp_rejected_report}" 2>/dev/null
aggregate_rejected_rc=$?
set -e
if [[ "${aggregate_rejected_rc}" -eq 0 ]] || [[ "$(jq -r '.counts.rejected_local_heavy' "${tmp_rejected_report}")" != "1" ]]; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "rejected_local_heavy" \
    "negative_guardrail_not_enforced" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_rejected_report}")" \
    "aggregate gate must reject local heavy Cargo without fallback approval"
  exit 1
fi

jq -c . "${tmp_missing_artifact}" > "${tmp_missing_artifact_ledger}"
set +e
"${VALIDATOR}" --aggregate-ledger "${tmp_missing_artifact_ledger}" > "${tmp_missing_artifact_report}" 2>/dev/null
aggregate_missing_rc=$?
set -e
if [[ "${aggregate_missing_rc}" -eq 0 ]] || [[ "$(jq -r '.counts.missing_artifact' "${tmp_missing_artifact_report}")" != "1" ]]; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "missing_artifact" \
    "negative_guardrail_not_enforced" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_missing_artifact_report}")" \
    "aggregate gate must reject missing proof artifact paths"
  exit 1
fi

printf '%s\n' "not-json" > "${tmp_malformed_ledger}"
set +e
"${VALIDATOR}" --aggregate-ledger "${tmp_malformed_ledger}" > "${tmp_malformed_report}" 2>/dev/null
aggregate_malformed_rc=$?
set -e
if [[ "${aggregate_malformed_rc}" -eq 0 ]] || [[ "$(jq -r '.counts.malformed' "${tmp_malformed_report}")" != "1" ]]; then
  emit_log \
    "failed" \
    "aggregate_quality_gate" \
    "malformed_jsonl" \
    "negative_guardrail_not_enforced" \
    "aggregate_report_mismatch" \
    "$(basename "${tmp_malformed_report}")" \
    "aggregate gate must reject malformed JSONL rows"
  exit 1
fi

emit_log \
  "passed" \
  "aggregate_quality_gate" \
  "mixed_ledger_partial_risk->rejected_local_heavy->missing_artifact->malformed_jsonl" \
  "aggregate_quality_gate_validated" \
  "none" \
  "$(basename "${tmp_mixed_report}")" \
  "aggregate quality gate categories and partial-risk verdict validated"

aggregate_closeout_summary="$(jq -c '{
  overall_verdict,
  quality_gate_passed,
  blocking_failure_count,
  counts,
  worker_evidence_counts
}' "${tmp_mixed_report}")"
emit_log \
  "passed" \
  "closeout_workflow_examples" \
  "remote_rch_proof->invalid_local_heavy_claim->approved_local_fallback->missing_artifact->aggregate_verdict" \
  "proof_ledger_closeout_summary_validated" \
  "none" \
  "$(basename "${tmp_mixed_report}")" \
  "${aggregate_closeout_summary}"

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

for required_term in \
  "remote RCH proof" \
  "light local proof" \
  "approved local fallback" \
  "invalid local-heavy claim" \
  "worker_evidence_confidence" \
  "target_worker_remote_proof" \
  "scheduler_selected_remote_proof" \
  "source_mirror_status" \
  "static-only check" \
  "blocked verifier"; do
  rg -Fq "${required_term}" "${POLICY_DOC}" || {
    emit_log \
      "failed" \
      "doc_wiring" \
      "policy_terminology_check" \
      "missing_closeout_term" \
      "doc_reference_missing" \
      "$(basename "${POLICY_DOC}")" \
      "policy doc missing proof-ledger closeout term: ${required_term}"
    exit 1
  }
done

for required_term in \
  "remote RCH proof" \
  "light local proof" \
  "approved local fallback" \
  "worker_evidence_confidence" \
  "target_worker_remote_proof" \
  "source_mirror_status" \
  "static-only check" \
  "blocked verifier" \
  "invalid local-heavy claim" \
  "Proof-ledger closeout summary"; do
  rg -Fq "${required_term}" "${VERIFICATION_DOC}" || {
    emit_log \
      "failed" \
      "doc_wiring" \
      "verification_terminology_check" \
      "missing_closeout_term" \
      "doc_reference_missing" \
      "$(basename "${VERIFICATION_DOC}")" \
      "finish-line verification contract missing proof-ledger closeout term: ${required_term}"
    exit 1
  }
done

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
