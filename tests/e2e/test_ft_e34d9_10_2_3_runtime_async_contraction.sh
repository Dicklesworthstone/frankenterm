#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_2_3"
CORRELATION_ID="ft-e34d9.10.2.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"

BASE_CARGO_TARGET_DIR="target/rch-e2e-ft-e34d9-10-2-3"
if [[ -n "${CARGO_TARGET_DIR:-}" && "${CARGO_TARGET_DIR}" == target/* ]]; then
  BASE_CARGO_TARGET_DIR="${CARGO_TARGET_DIR}"
fi
CARGO_TARGET_DIR="${BASE_CARGO_TARGET_DIR%/}-${RUN_ID}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""

# shellcheck disable=SC1091
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "e34d9_10_2_3_runtime_async_contraction"

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

record_rch_preflight_artifacts() {
  if [[ -f "${_RCH_PROBE_LOG}" ]]; then
    emit_log "preflight" "rch_preflight.probe" "artifact=$(basename "${_RCH_PROBE_LOG}")" "captured" "rch_probe_artifact" "none" "$(basename "${_RCH_PROBE_LOG}")"
  fi
  if [[ -f "${_RCH_SMOKE_LOG}" ]]; then
    emit_log "preflight" "rch_preflight.smoke" "artifact=$(basename "${_RCH_SMOKE_LOG}")" "captured" "rch_smoke_artifact" "none" "$(basename "${_RCH_SMOKE_LOG}")"
  fi
}

ensure_rch_ready_capture_artifacts() {
  local rc=0
  set +e
  ( ensure_rch_ready )
  rc=$?
  set -e
  record_rch_preflight_artifacts
  return "${rc}"
}

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return "${rc}"
}

run_rch_cargo_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  (
    run_rch_cargo_logged "${LAST_STEP_LOG}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
  )
  local rc=$?
  set -e

  if [[ -f "${LAST_STEP_LOG}" ]]; then
    tee -a "${STDOUT_FILE}" < "${LAST_STEP_LOG}" >/dev/null
  fi
  return "${rc}"
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

run_rch_test_step() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_rch_cargo_step "${label}" "$@"; then
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "test_failure" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

validate_spawn_blocking_allowlist() {
  local mode="$1"
  local output_file="$2"
  local pattern="runtime_async::task::spawn_blocking"

  # The nominal contract bans transitional task::spawn_blocking callsites outside
  # runtime_async.rs. For failure injection, deliberately widen the detector to
  # the canonical helper so the script still proves detector sensitivity even
  # after the transitional helper count reaches zero.
  if [[ "${mode}" == "failure_injection" ]]; then
    pattern="runtime_async::spawn_blocking"
  fi

  rg -n "${pattern}" \
    crates/frankenterm/src/main.rs \
    crates/frankenterm-core/src \
    > "${output_file}" || true

  local unexpected=0
  while IFS= read -r line; do
    [[ -z "${line}" ]] && continue
    case "${line}" in
      crates/frankenterm-core/src/search_bridge.rs:*)
        ;;
      *)
        unexpected=1
        ;;
    esac
  done < "${output_file}"

  if [[ "${mode}" == "nominal" ]]; then
    [[ "${unexpected}" -eq 0 ]]
    return
  fi

  # Failure-injection mode: use the broader canonical helper pattern with an
  # intentionally empty allowlist so the detector path still fires against real
  # code after the transitional helper count reaches zero.
  [[ -s "${output_file}" ]]
}

# Validates that runtime_async helper functions and `runtime_async::process::Command`
# are NOT called outside of runtime_async.rs itself. Function was historically named
# after the legacy `runtime_compat` module (renamed under ft-g43fq, alias removed
# under ft-y378j.4); the actual check has always validated the canonical
# runtime_async surface contract. Renamed for accuracy.
validate_runtime_async_helper_callsites() {
  local output_file="$1"
  rg -n "runtime_async::process::Command|\\b(mpsc_recv_option|mpsc_send|watch_has_changed|watch_borrow_and_update_clone|watch_changed)\\s*\\(" \
    crates/frankenterm/src/main.rs \
    crates/frankenterm-core/src \
    --glob '!runtime_async.rs' \
    > "${output_file}" || true
  [[ ! -s "${output_file}" ]]
}

run_static_contract_checks() {
  local allowlist_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_allowlist_nominal.log"
  if validate_spawn_blocking_allowlist "nominal" "${allowlist_log}"; then
    emit_log "validation" "compat_surface.allowlist.nominal" "allowed=search_bridge_only" "passed" "allowlist_enforced" "none" "$(basename "${allowlist_log}")"
  else
    emit_log "validation" "compat_surface.allowlist.nominal" "allowed=search_bridge_only" "failed" "unexpected_spawn_blocking_callsite" "SURFACE-E200" "$(basename "${allowlist_log}")"
    cat "${allowlist_log}" >&2
    exit 1
  fi

  local failure_injection_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_allowlist_failure_injection.log"
  if validate_spawn_blocking_allowlist "failure_injection" "${failure_injection_log}"; then
    emit_log "validation" "compat_surface.allowlist.failure_injection" "pattern=runtime_async::spawn_blocking;allowed=none" "passed" "detector_triggered_expected_failure" "none" "$(basename "${failure_injection_log}")"
  else
    emit_log "validation" "compat_surface.allowlist.failure_injection" "pattern=runtime_async::spawn_blocking;allowed=none" "failed" "detector_missed_expected_failure" "SURFACE-E201" "$(basename "${failure_injection_log}")"
    exit 1
  fi

  local recovery_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_allowlist_recovery.log"
  if validate_spawn_blocking_allowlist "nominal" "${recovery_log}"; then
    emit_log "validation" "compat_surface.allowlist.recovery" "allowed=search_bridge_only" "passed" "recovery_check_passed" "none" "$(basename "${recovery_log}")"
  else
    emit_log "validation" "compat_surface.allowlist.recovery" "allowed=search_bridge_only" "failed" "recovery_check_failed" "SURFACE-E202" "$(basename "${recovery_log}")"
    cat "${recovery_log}" >&2
    exit 1
  fi

  local helper_guard_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_runtime_async_helpers.log"
  if validate_runtime_async_helper_callsites "${helper_guard_log}"; then
    emit_log "validation" "compat_surface.helper_callsites.nominal" "expected=zero_runtime_async_helper_or_process_callsites_outside_runtime_async_rs" "passed" "runtime_async_helper_replacement_enforced" "none" "$(basename "${helper_guard_log}")"
  else
    emit_log "validation" "compat_surface.helper_callsites.nominal" "expected=zero_runtime_async_helper_or_process_callsites_outside_runtime_async_rs" "failed" "unexpected_runtime_async_helper_callsite" "SURFACE-E203" "$(basename "${helper_guard_log}")"
    cat "${helper_guard_log}" >&2
    exit 1
  fi
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rg
require_cmd rch
require_cmd cargo

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"
emit_log "preflight" "target_dir" "cargo_target_dir=${CARGO_TARGET_DIR}" "configured" "none" "none" "$(basename "${LOG_FILE}")"

if ensure_rch_ready_capture_artifacts; then
  emit_log "preflight" "rch_preflight" "ensure_rch_ready" "passed" "rch_preflight_passed" "none" "$(basename "${_RCH_SMOKE_LOG}")"
  emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${_RCH_PROBE_LOG}")"
else
  emit_log "preflight" "rch_preflight" "ensure_rch_ready" "failed" "rch_preflight_failed" "RCH-E100" "$(basename "${_RCH_SMOKE_LOG}")"
  exit 2
fi

run_static_contract_checks

run_rch_test_step \
  "runtime_async_surface_guard_unit" \
  "runtime_async.surface_guard.unit" \
  "test=runtime_async_surface_guard::tests::allowed_raw_runtime_files_contains_only_runtime_async_and_cx" \
  test -p frankenterm-core --lib runtime_async_surface_guard::tests::allowed_raw_runtime_files_contains_only_runtime_async_and_cx -- --nocapture

run_rch_test_step \
  "runtime_async_smoke" \
  "runtime_async.smoke.integration" \
  "test_target=runtime_async_smoke" \
  test -p frankenterm-core --test runtime_async_smoke -- --nocapture

emit_log "summary" "nominal_suite" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
echo "ft-e34d9.10.2.3 runtime_async contraction scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
