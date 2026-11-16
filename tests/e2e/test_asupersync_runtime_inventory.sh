#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_1_1_runtime_inventory"
CORRELATION_ID="ft-e34d9.10.1.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/asupersync_runtime_inventory_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/asupersync_runtime_inventory_${RUN_ID}.stdout.log"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

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
    --arg component "asupersync_runtime_inventory.e2e" \
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
    }' | tee -a "${LOG_FILE}"
}

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "Starting asupersync runtime inventory e2e validation"

if ! command -v python3 >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "missing_python3" \
    "python3_not_installed" \
    "$(basename "${LOG_FILE}")" \
    "python3 is required"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

GENERATOR="${ROOT_DIR}/scripts/generate_asupersync_runtime_inventory.sh"
if [[ ! -x "${GENERATOR}" ]]; then
  emit_log \
    "failed" \
    "execution_preflight" \
    "missing_generator" \
    "generator_missing_or_not_executable" \
    "$(basename "${LOG_FILE}")" \
    "generator script missing or not executable: ${GENERATOR}"
  exit 1
fi

TMP_ONE="$(mktemp)"
TMP_TWO="$(mktemp)"
NORM_ONE="$(mktemp)"
NORM_TWO="$(mktemp)"
NORM_DOC="$(mktemp)"
cleanup() {
  rm -f "${TMP_ONE}" "${TMP_TWO}" "${NORM_ONE}" "${NORM_TWO}" "${NORM_DOC}"
}
trap cleanup EXIT

emit_log \
  "running" \
  "inventory_generation_first" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Generating first inventory snapshot"
"${GENERATOR}" "${TMP_ONE}" 2>&1 | tee -a "${STDOUT_FILE}"

emit_log \
  "running" \
  "generator_self_test" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Running generator parser/classifier self-tests"
"${GENERATOR}" --self-test "${TMP_ONE}" 2>&1 | tee -a "${STDOUT_FILE}"

emit_log \
  "running" \
  "inventory_generation_second" \
  "none" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "Generating second inventory snapshot for determinism check"
"${GENERATOR}" "${TMP_TWO}" 2>&1 | tee -a "${STDOUT_FILE}"

if ! jq -e '.pattern_reference_counts.tokio > 0' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_tokio_references" \
    "expected_field_value_missing" \
    "$(basename "${TMP_ONE}")" \
    "expected tokio references > 0"
  exit 1
fi

if ! jq -e '.usage_by_crate_root[] | select(.crate_root == "crates/frankenterm-core")' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_crate_usage_row" \
    "expected_field_value_missing" \
    "$(basename "${TMP_ONE}")" \
    "expected crates/frankenterm-core row in usage_by_crate_root"
  exit 1
fi

if ! jq -e '.top_runtime_reference_files | length > 0' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_top_runtime_files" \
    "expected_field_value_missing" \
    "$(basename "${TMP_ONE}")" \
    "expected non-empty top_runtime_reference_files"
  exit 1
fi

if ! jq -e '.migration_classification | length > 0' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_migration_classification" \
    "expected_field_value_missing" \
    "$(basename "${TMP_ONE}")" \
    "expected non-empty migration_classification"
  exit 1
fi

if ! jq -e '.migration_classification[] | select(.criticality == "high" or .criticality == "medium" or .criticality == "low")' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "invalid_criticality_label" \
    "schema_validation_failed" \
    "$(basename "${TMP_ONE}")" \
    "expected valid criticality labels in migration_classification"
  exit 1
fi

if ! jq -e '.migration_classification[] | select((.affected_user_workflows | type) == "array" and (.affected_user_workflows | length) > 0)' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_affected_user_workflows" \
    "schema_validation_failed" \
    "$(basename "${TMP_ONE}")" \
    "expected affected_user_workflows arrays in migration_classification"
  exit 1
fi

if ! jq -e '.symbol_reference_counts.tokio_spawn >= 0 and .symbol_reference_counts.runtime_compat_sleep >= 0' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_symbol_reference_counts" \
    "schema_validation_failed" \
    "$(basename "${TMP_ONE}")" \
    "expected symbol_reference_counts to include token probes"
  exit 1
fi

if ! jq -e '.symbol_occurrence_top_files | type == "array"' "${TMP_ONE}" >/dev/null; then
  emit_log \
    "failed" \
    "assertion_check" \
    "missing_symbol_occurrence_top_files" \
    "schema_validation_failed" \
    "$(basename "${TMP_ONE}")" \
    "expected symbol_occurrence_top_files array"
  exit 1
fi

jq 'del(.generated_at)' "${TMP_ONE}" > "${NORM_ONE}"
jq 'del(.generated_at)' "${TMP_TWO}" > "${NORM_TWO}"

if ! diff -u "${NORM_ONE}" "${NORM_TWO}" >/dev/null; then
  emit_log \
    "failed" \
    "determinism_check" \
    "non_deterministic_output" \
    "inventory_diff_detected" \
    "$(basename "${STDOUT_FILE}")" \
    "inventory output is not stable across consecutive runs"
  exit 1
fi

DOC_PATH="${ROOT_DIR}/docs/asupersync-runtime-inventory.json"
if [[ -f "${DOC_PATH}" ]]; then
  jq 'del(.generated_at)' "${DOC_PATH}" > "${NORM_DOC}"
  if ! diff -u "${NORM_ONE}" "${NORM_DOC}" >/dev/null; then
    emit_log \
      "failed" \
      "doc_drift_check" \
      "inventory_doc_outdated" \
      "docs_inventory_drift" \
      "$(basename "${DOC_PATH}")" \
      "docs/asupersync-runtime-inventory.json is out of date; regenerate before commit"
    exit 1
  fi
fi

emit_log \
  "passed" \
  "generation->self_test->schema_assertions->determinism->doc_drift_guard" \
  "runtime_inventory_validated" \
  "none" \
  "$(basename "${STDOUT_FILE}")" \
  "asupersync runtime inventory e2e validation completed"

echo "Asupersync runtime inventory e2e passed. Logs: ${LOG_FILE_REL}"
