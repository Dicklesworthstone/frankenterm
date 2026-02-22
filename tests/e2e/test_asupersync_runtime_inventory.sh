#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
LOG_FILE="${LOG_DIR}/asupersync_runtime_inventory_${RUN_ID}.log"

log_json() {
  local level="$1"
  local event="$2"
  local message="$3"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"ts":"%s","level":"%s","event":"%s","message":"%s"}\n' \
    "${now}" "${level}" "${event}" "${message}" | tee -a "${LOG_FILE}"
}

log_json "info" "start" "Starting asupersync runtime inventory e2e validation"
log_json "info" "context" "root=${ROOT_DIR} log=${LOG_FILE}"

if ! command -v python3 >/dev/null 2>&1; then
  log_json "error" "missing_python3" "python3 is required"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  log_json "error" "missing_jq" "jq is required"
  exit 1
fi

GENERATOR="${ROOT_DIR}/scripts/generate_asupersync_runtime_inventory.sh"
if [[ ! -x "${GENERATOR}" ]]; then
  log_json "error" "missing_generator" "generator script missing or not executable: ${GENERATOR}"
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

log_json "info" "generate_first" "Generating first inventory snapshot"
"${GENERATOR}" "${TMP_ONE}" 2>&1 | tee -a "${LOG_FILE}"

log_json "info" "self_test" "Running generator self-tests"
"${GENERATOR}" --self-test "${TMP_ONE}" 2>&1 | tee -a "${LOG_FILE}"

log_json "info" "generate_second" "Generating second inventory snapshot"
"${GENERATOR}" "${TMP_TWO}" 2>&1 | tee -a "${LOG_FILE}"

if ! jq -e '.pattern_reference_counts.tokio > 0' "${TMP_ONE}" >/dev/null; then
  log_json "error" "assertion_failed" "expected tokio references > 0"
  exit 1
fi

if ! jq -e '.usage_by_crate_root[] | select(.crate_root == "crates/frankenterm-core")' "${TMP_ONE}" >/dev/null; then
  log_json "error" "assertion_failed" "expected crates/frankenterm-core row in usage_by_crate_root"
  exit 1
fi

if ! jq -e '.top_runtime_reference_files | length > 0' "${TMP_ONE}" >/dev/null; then
  log_json "error" "assertion_failed" "expected non-empty top_runtime_reference_files"
  exit 1
fi

if ! jq -e '.migration_classification | length > 0' "${TMP_ONE}" >/dev/null; then
  log_json "error" "assertion_failed" "expected non-empty migration_classification"
  exit 1
fi

if ! jq -e '.migration_classification[] | select(.criticality == "high" or .criticality == "medium" or .criticality == "low")' "${TMP_ONE}" >/dev/null; then
  log_json "error" "assertion_failed" "expected valid criticality labels in migration_classification"
  exit 1
fi

jq 'del(.generated_at)' "${TMP_ONE}" > "${NORM_ONE}"
jq 'del(.generated_at)' "${TMP_TWO}" > "${NORM_TWO}"

if ! diff -u "${NORM_ONE}" "${NORM_TWO}" >/dev/null; then
  log_json "error" "non_deterministic" "inventory output is not stable across consecutive runs"
  exit 1
fi

DOC_PATH="${ROOT_DIR}/docs/asupersync-runtime-inventory.json"
if [[ -f "${DOC_PATH}" ]]; then
  jq 'del(.generated_at)' "${DOC_PATH}" > "${NORM_DOC}"
  if ! diff -u "${NORM_ONE}" "${NORM_DOC}" >/dev/null; then
    log_json "error" "doc_drift" "docs/asupersync-runtime-inventory.json is out of date; regenerate before commit"
    exit 1
  fi
fi

log_json "info" "success" "Asupersync runtime inventory e2e validation completed"
