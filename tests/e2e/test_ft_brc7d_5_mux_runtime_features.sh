#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_brc7d_5_mux_runtime_features"
CORRELATION_ID="ft-brc7d.5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
TARGET_DIR="target-rch-ft-brc7d-5-${RUN_ID}"

LAST_STEP_LOG=""

emit_log() {
  local component="$1"
  local decision_path="$2"
  local input_summary="$3"
  local outcome="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "${component}" \
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

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "dependency_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  (
    cd "${ROOT_DIR}"
    "$@"
  ) 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return ${rc}
}

record_rch_execution_mode() {
  local decision_path="$1"
  local artifact
  artifact="$(basename "${LAST_STEP_LOG}")"
  if grep -q "\\[RCH\\] local" "${LAST_STEP_LOG}"; then
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_fail_open_local_fallback" "none" "${artifact}"
  else
    emit_log "validation" "${decision_path}.rch_mode" "rch_exec_mode" "passed" "rch_remote_offload" "none" "${artifact}"
  fi
}

run_rch_expect_success() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"; then
    record_rch_execution_mode "${decision_path}"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "command_succeeded" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "unexpected_command_failure" "CARGO-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

run_rch_expect_failure() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" "$@"; then
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "failure_injection_did_not_fail" "EXPECTED-FAIL-MISSING" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  else
    record_rch_execution_mode "${decision_path}"
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "expected_failure_observed" "none" "$(basename "${LAST_STEP_LOG}")"
  fi
}

assert_log_contains() {
  local decision_path="$1"
  local pattern="$2"

  if rg -n "${pattern}" "${LAST_STEP_LOG}" >/dev/null 2>&1; then
    emit_log "validation" "${decision_path}" "pattern=${pattern}" "passed" "expected_error_signature_present" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}" "pattern=${pattern}" "failed" "expected_error_signature_missing" "EXPECTED-PATTERN-MISSING" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rg
require_cmd rch
require_cmd cargo

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"
emit_log "preflight" "cargo_target" "target_dir=${TARGET_DIR}" "configured" "none" "none" "$(basename "${LOG_FILE}")"

run_rch_expect_success \
  "nominal_default_runtime" \
  "runtime_feature.nominal_default" \
  "cargo check -p mux --all-targets" \
  cargo check -p mux --all-targets

run_rch_expect_failure \
  "failure_injection_dual_runtime" \
  "runtime_feature.failure_injection_dual_runtime" \
  "cargo check -p mux --all-targets --no-default-features --features async-smol,async-asupersync,no-lua" \
  cargo check -p mux --all-targets --no-default-features --features async-smol,async-asupersync,no-lua

assert_log_contains \
  "runtime_feature.failure_injection_dual_runtime.signature" \
  "mutually exclusive"

run_rch_expect_success \
  "recovery_preferred_runtime" \
  "runtime_feature.recovery_preferred" \
  "cargo check -p mux --all-targets --no-default-features --features async-asupersync,no-lua" \
  cargo check -p mux --all-targets --no-default-features --features async-asupersync,no-lua

run_rch_expect_success \
  "recovery_legacy_runtime" \
  "runtime_feature.recovery_legacy" \
  "cargo check -p mux --all-targets --no-default-features --features async-smol,no-lua" \
  cargo check -p mux --all-targets --no-default-features --features async-smol,no-lua

run_rch_expect_success \
  "targeted_test_preferred_runtime" \
  "runtime_feature.test_preferred" \
  "cargo test -p mux --no-default-features --features async-asupersync,no-lua split_source_spawn_no_command -- --nocapture" \
  cargo test -p mux --no-default-features --features async-asupersync,no-lua split_source_spawn_no_command -- --nocapture

run_rch_expect_success \
  "targeted_test_legacy_runtime" \
  "runtime_feature.test_legacy" \
  "cargo test -p mux --no-default-features --features async-smol,no-lua split_source_spawn_no_command -- --nocapture" \
  cargo test -p mux --no-default-features --features async-smol,no-lua split_source_spawn_no_command -- --nocapture

emit_log "summary" "scenario_complete" "all_runtime_feature_checks_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"

echo "ft-brc7d.5 mux runtime feature scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
