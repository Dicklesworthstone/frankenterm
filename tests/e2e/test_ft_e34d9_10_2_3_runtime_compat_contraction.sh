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

BASE_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft-e34d9-10-2-3}"
CARGO_TARGET_DIR="${BASE_CARGO_TARGET_DIR%/}-${RUN_ID}"
export CARGO_TARGET_DIR

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

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return ${rc}
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

ensure_rch_remote_only() {
  if grep -q "\[RCH\] local" "${LAST_STEP_LOG}"; then
    emit_log "validation" "rch_offload_policy" "rch_local_fallback_detected" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
}

run_rch_test_step() {
  local label="$1"
  local decision_path="$2"
  local input_summary="$3"
  shift 3

  emit_log "validation" "${decision_path}" "${input_summary}" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
  if run_step "${label}" rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "$@"; then
    ensure_rch_remote_only
    emit_log "validation" "${decision_path}" "${input_summary}" "passed" "tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
  else
    emit_log "validation" "${decision_path}" "${input_summary}" "failed" "test_failure" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
    exit 1
  fi
}

validate_spawn_blocking_allowlist() {
  local mode="$1"
  local output_file="$2"

  rg -n "runtime_compat::task::spawn_blocking" crates/frankenterm-core/src > "${output_file}" || true

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

  # Failure-injection mode: enforce an intentionally empty allowlist and
  # require the check to fail (proves detector sensitivity).
  [[ -s "${output_file}" ]]
}

validate_runtime_compat_helper_callsites() {
  local output_file="$1"
  rg -n "\\b(mpsc_recv_option|mpsc_send|watch_has_changed|watch_borrow_and_update_clone|watch_changed)\\b" \
    crates/frankenterm-core/src \
    --glob '!runtime_compat.rs' \
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
    emit_log "validation" "compat_surface.allowlist.failure_injection" "allowed=none" "passed" "detector_triggered_expected_failure" "none" "$(basename "${failure_injection_log}")"
  else
    emit_log "validation" "compat_surface.allowlist.failure_injection" "allowed=none" "failed" "detector_missed_expected_failure" "SURFACE-E201" "$(basename "${failure_injection_log}")"
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

  local helper_guard_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_runtime_compat_helpers.log"
  if validate_runtime_compat_helper_callsites "${helper_guard_log}"; then
    emit_log "validation" "compat_surface.helper_callsites.nominal" "expected=zero_runtime_compat_helper_callsites_outside_runtime_compat_rs" "passed" "runtime_compat_helper_replacement_enforced" "none" "$(basename "${helper_guard_log}")"
  else
    emit_log "validation" "compat_surface.helper_callsites.nominal" "expected=zero_runtime_compat_helper_callsites_outside_runtime_compat_rs" "failed" "unexpected_runtime_compat_helper_callsite" "SURFACE-E203" "$(basename "${helper_guard_log}")"
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
run_static_contract_checks

probe_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_probe.json"
status_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_status.json"
check_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_check.log"
set +e
rch check > "${check_log}" 2>&1
check_rc=$?
set -e

if [[ ${check_rc} -eq 0 ]]; then
  emit_log "preflight" "rch_check" "rch_check" "passed" "rch_check_ready" "none" "$(basename "${check_log}")"
else
  emit_log "preflight" "rch_check" "rch_check" "failed" "rch_check_failed" "RCH-E000" "$(basename "${check_log}")"
fi

set +e
rch workers probe --all --json > "${probe_log}" 2>>"${STDOUT_FILE}"
probe_rc=$?
set -e

probe_reachable="false"
if [[ ${probe_rc} -eq 0 ]]; then
  healthy_workers=$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${probe_log}")
  if [[ "${healthy_workers}" -ge 1 ]]; then
    probe_reachable="true"
  fi
fi

if [[ "${probe_reachable}" == "true" ]]; then
  emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"
else
  probe_reason_code="rch_workers_unreachable_probe"
  probe_error_code="RCH-E100"
  if [[ ${check_rc} -eq 0 ]]; then
    probe_reason_code="rch_health_probe_mismatch"
    probe_error_code="RCH-E101"
  fi

  if rch --json status --workers --jobs > "${status_log}" 2>>"${STDOUT_FILE}"; then
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "${probe_reason_code}" "${probe_error_code}" "$(basename "${status_log}")"
  else
    emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_status_unavailable" "${probe_error_code}" "$(basename "${status_log}")"
  fi
  echo "workers probe found no reachable remote workers; refusing local fallback" >&2
  exit 2
fi

run_rch_test_step \
  "runtime_compat_surface_contract_unit" \
  "runtime_compat.surface_contract.unit" \
  "test=runtime_compat::tests::surface_contract_entries_are_unique" \
  cargo test -p frankenterm-core runtime_compat::tests::surface_contract_entries_are_unique -- --nocapture

run_rch_test_step \
  "runtime_compat_smoke" \
  "runtime_compat.smoke.integration" \
  "test_target=runtime_compat_smoke" \
  cargo test -p frankenterm-core --test runtime_compat_smoke -- --nocapture

emit_log "summary" "nominal_suite" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
echo "ft-e34d9.10.2.3 runtime_compat contraction scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
