#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_1_2_runtime_doctrine"
CORRELATION_ID="ft-e34d9.10.1.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_runtime_doctrine_${RUN_ID}.jsonl"

DOCTRINE_PACK="${ROOT_DIR}/docs/asupersync-runtime-invariants.json"
ADR_FILE="${ROOT_DIR}/docs/adr/0012-asupersync-runtime-doctrine.md"
BASELINE_FILE="${ROOT_DIR}/docs/asupersync-migration-baseline.md"
PLAYBOOK_FILE="${ROOT_DIR}/docs/asupersync-migration-playbook.md"
ARCH_FILE="${ROOT_DIR}/docs/architecture.md"
INVENTORY_FILE="${ROOT_DIR}/docs/asupersync-runtime-inventory.json"

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
    --arg component "asupersync_runtime_doctrine.e2e" \
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

validate_doctrine_pack() {
  local pack_path="$1"

  jq -e '.doctrine_version == "1.0.0"' "${pack_path}" >/dev/null || return 1
  jq -e '.bead_id == "ft-e34d9.10.1.2"' "${pack_path}" >/dev/null || return 1
  jq -e '.adr_ref == "docs/adr/0012-asupersync-runtime-doctrine.md"' "${pack_path}" >/dev/null || return 1

  jq -e '.invariants | type == "array" and length >= 5' "${pack_path}" >/dev/null || return 1
  jq -e '.invariants[] | select(.id == "INV-001")' "${pack_path}" >/dev/null || return 1
  jq -e '.invariants[] | select(.id == "INV-002")' "${pack_path}" >/dev/null || return 1
  jq -e '.invariants[] | select(.id == "INV-003")' "${pack_path}" >/dev/null || return 1
  jq -e '.invariants[] | select(.id == "INV-004")' "${pack_path}" >/dev/null || return 1
  jq -e '.invariants[] | select(.id == "INV-005")' "${pack_path}" >/dev/null || return 1

  jq -e '.anti_patterns | type == "array" and length >= 5' "${pack_path}" >/dev/null || return 1
  jq -e '.anti_patterns[] | select(.id == "AP-001")' "${pack_path}" >/dev/null || return 1
  jq -e '.anti_patterns[] | select(.id == "AP-002")' "${pack_path}" >/dev/null || return 1
  jq -e '.anti_patterns[] | select(.id == "AP-003")' "${pack_path}" >/dev/null || return 1
  jq -e '.anti_patterns[] | select(.id == "AP-004")' "${pack_path}" >/dev/null || return 1
  jq -e '.anti_patterns[] | select(.id == "AP-005")' "${pack_path}" >/dev/null || return 1

  jq -e '.legacy_to_target_map[] | select(.legacy_pattern == "tokio::spawn" and (.user_visible_change | length) > 0)' "${pack_path}" >/dev/null || return 1
  jq -e '.legacy_to_target_map[] | select(.legacy_pattern == "tokio::select!" and (.user_visible_change | length) > 0)' "${pack_path}" >/dev/null || return 1
  jq -e '.legacy_to_target_map[] | select(.legacy_pattern == "tokio::time::sleep/timeout" and (.user_visible_change | length) > 0)' "${pack_path}" >/dev/null || return 1
  jq -e '.legacy_to_target_map[] | select(.legacy_pattern == "lossy channel/send/write sequences" and (.user_visible_change | length) > 0)' "${pack_path}" >/dev/null || return 1
  jq -e '.legacy_to_target_map[] | select(.legacy_pattern == "ambient runtime bootstrap" and (.user_visible_change | length) > 0)' "${pack_path}" >/dev/null || return 1

  jq -e '.user_facing_guarantees[] | select(.id == "UG-001")' "${pack_path}" >/dev/null || return 1
  jq -e '.user_facing_guarantees[] | select(.id == "UG-002")' "${pack_path}" >/dev/null || return 1
  jq -e '.user_facing_guarantees[] | select(.id == "UG-003")' "${pack_path}" >/dev/null || return 1

  jq -e '.structured_log_contract.component == "asupersync_runtime_doctrine.e2e"' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.secret_safe_redaction_required == true' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("timestamp")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("component")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("scenario_id")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("correlation_id")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("decision_path")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("input_summary")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("outcome")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("reason_code")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("error_code")' "${pack_path}" >/dev/null || return 1
  jq -e '.structured_log_contract.required_fields | index("artifact_path")' "${pack_path}" >/dev/null || return 1

  return 0
}

emit_log \
  "started" \
  "suite_init" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.2 runtime doctrine contract validation"

if ! command -v jq >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_jq" \
    "jq_missing" \
    "jq_not_found" \
    "$(basename "${LOG_FILE}")" \
    "jq is required for doctrine validation"
  exit 1
fi

if ! command -v rg >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "suite_init" \
    "preflight_rg" \
    "ripgrep_missing" \
    "rg_not_found" \
    "$(basename "${LOG_FILE}")" \
    "rg is required for doctrine validation"
  exit 1
fi

for artifact in "${DOCTRINE_PACK}" "${ADR_FILE}" "${BASELINE_FILE}" "${PLAYBOOK_FILE}" "${ARCH_FILE}" "${INVENTORY_FILE}"; do
  if [[ ! -f "${artifact}" ]]; then
    emit_log \
      "failed" \
      "suite_init" \
      "preflight_artifacts" \
      "missing_artifact" \
      "artifact_not_found" \
      "${artifact}" \
      "required doctrine artifact missing"
    exit 1
  fi
done

emit_log \
  "running" \
  "unit_contract" \
  "doctrine_pack_validation" \
  "none" \
  "none" \
  "$(basename "${DOCTRINE_PACK}")" \
  "validating doctrine pack schema and contract IDs"

if ! validate_doctrine_pack "${DOCTRINE_PACK}"; then
  emit_log \
    "failed" \
    "unit_contract" \
    "doctrine_pack_validation" \
    "invalid_doctrine_pack" \
    "schema_or_contract_check_failed" \
    "$(basename "${DOCTRINE_PACK}")" \
    "canonical doctrine pack failed validation"
  exit 1
fi

emit_log \
  "passed" \
  "unit_contract" \
  "doctrine_pack_validation" \
  "doctrine_pack_valid" \
  "none" \
  "$(basename "${DOCTRINE_PACK}")" \
  "canonical doctrine pack passed validation"

emit_log \
  "running" \
  "integration_docs" \
  "reference_wiring_check" \
  "none" \
  "none" \
  "$(basename "${BASELINE_FILE}")" \
  "verifying doctrine contract references are wired in baseline/playbook/architecture docs"

rg -q "0012-asupersync-runtime-doctrine" "${BASELINE_FILE}" "${PLAYBOOK_FILE}" "${ARCH_FILE}" "${ADR_FILE}" || {
  emit_log \
    "failed" \
    "integration_docs" \
    "reference_wiring_check" \
    "missing_adr_reference" \
    "doctrine_reference_missing" \
    "$(basename "${BASELINE_FILE}")" \
    "expected ADR references missing from doctrine docs"
  exit 1
}

rg -q "asupersync-runtime-invariants.json" "${BASELINE_FILE}" "${PLAYBOOK_FILE}" "${ARCH_FILE}" || {
  emit_log \
    "failed" \
    "integration_docs" \
    "reference_wiring_check" \
    "missing_invariants_pack_reference" \
    "invariants_reference_missing" \
    "$(basename "${BASELINE_FILE}")" \
    "expected invariants pack references missing"
  exit 1
}

rg -q "test_asupersync_runtime_doctrine.sh" "${BASELINE_FILE}" || {
  emit_log \
    "failed" \
    "integration_docs" \
    "reference_wiring_check" \
    "missing_doctrine_e2e_reference" \
    "e2e_reference_missing" \
    "$(basename "${BASELINE_FILE}")" \
    "baseline missing doctrine e2e command reference"
  exit 1
}

emit_log \
  "passed" \
  "integration_docs" \
  "reference_wiring_check" \
  "docs_reference_wiring_valid" \
  "none" \
  "$(basename "${BASELINE_FILE}")" \
  "doctrine references are wired across docs"

emit_log \
  "running" \
  "integration_inventory" \
  "inventory_surface_check" \
  "none" \
  "none" \
  "$(basename "${INVENTORY_FILE}")" \
  "verifying representative runtime surfaces remain tracked in inventory artifact"

jq -e '.top_runtime_reference_files[] | select(.path == "crates/frankenterm-core/src/runtime_compat.rs")' "${INVENTORY_FILE}" >/dev/null || {
  emit_log \
    "failed" \
    "integration_inventory" \
    "inventory_surface_check" \
    "missing_runtime_compat_surface" \
    "inventory_surface_not_found" \
    "$(basename "${INVENTORY_FILE}")" \
    "runtime_compat surface missing from top runtime references"
  exit 1
}

jq -e '.top_runtime_reference_files[] | select(.path == "crates/frankenterm/src/main.rs")' "${INVENTORY_FILE}" >/dev/null || {
  emit_log \
    "failed" \
    "integration_inventory" \
    "inventory_surface_check" \
    "missing_cli_runtime_surface" \
    "inventory_surface_not_found" \
    "$(basename "${INVENTORY_FILE}")" \
    "CLI runtime surface missing from top runtime references"
  exit 1
}

jq -e '.top_runtime_reference_files[] | select(.path == "crates/frankenterm-core/src/ipc.rs")' "${INVENTORY_FILE}" >/dev/null || {
  emit_log \
    "failed" \
    "integration_inventory" \
    "inventory_surface_check" \
    "missing_ipc_runtime_surface" \
    "inventory_surface_not_found" \
    "$(basename "${INVENTORY_FILE}")" \
    "IPC runtime surface missing from top runtime references"
  exit 1
}

emit_log \
  "passed" \
  "integration_inventory" \
  "inventory_surface_check" \
  "inventory_surfaces_present" \
  "none" \
  "$(basename "${INVENTORY_FILE}")" \
  "representative runtime surfaces are present in inventory"

tmp_negative="$(mktemp)"
cleanup() {
  rm -f "${tmp_negative}"
}
trap cleanup EXIT

jq '
  .legacy_to_target_map |= map(select(.legacy_pattern != "tokio::select!"))
  | .user_facing_guarantees |= map(select(.id != "UG-001"))
' "${DOCTRINE_PACK}" > "${tmp_negative}"

emit_log \
  "running" \
  "failure_injection" \
  "negative_guardrail" \
  "none" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "negative case should fail when doctrine pack is missing required mapping/guarantee"

if validate_doctrine_pack "${tmp_negative}" >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "failure_injection" \
    "negative_guardrail" \
    "guardrail_not_enforced" \
    "unexpected_negative_pass" \
    "$(basename "${tmp_negative}")" \
    "negative doctrine pack unexpectedly passed validation"
  exit 1
fi

emit_log \
  "passed" \
  "failure_injection" \
  "negative_guardrail" \
  "negative_guardrail_enforced" \
  "none" \
  "$(basename "${tmp_negative}")" \
  "negative doctrine pack correctly rejected"

emit_log \
  "running" \
  "recovery_validation" \
  "recovery_guardrail" \
  "none" \
  "none" \
  "$(basename "${DOCTRINE_PACK}")" \
  "recovery case should pass with canonical doctrine pack"

if ! validate_doctrine_pack "${DOCTRINE_PACK}"; then
  emit_log \
    "failed" \
    "recovery_validation" \
    "recovery_guardrail" \
    "unexpected_recovery_fail" \
    "canonical_pack_rejected" \
    "$(basename "${DOCTRINE_PACK}")" \
    "canonical doctrine pack failed recovery validation"
  exit 1
fi

emit_log \
  "passed" \
  "recovery_validation" \
  "recovery_guardrail" \
  "recovery_path_validated" \
  "none" \
  "$(basename "${DOCTRINE_PACK}")" \
  "recovery validation passed with canonical doctrine pack"

emit_log \
  "passed" \
  "suite_complete" \
  "unit_contract->integration_docs->integration_inventory->failure_injection->recovery_validation" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "ft-e34d9.10.1.2 doctrine validation succeeded"

echo "ft-e34d9.10.1.2 doctrine e2e validation passed. Log: ${LOG_FILE}"
