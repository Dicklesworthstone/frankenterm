#!/usr/bin/env bash
# E2E: Validate ft-e34d9.10.5.4 runtime-compat async surface guard contract.
#
# Scenarios:
#   1. Direct tokio async runtime primitives remain confined to runtime_async.rs
#   2. Failure injection proves the detector trips when the allowlist is removed
#   3. Recovery restores the nominal allowlist contract
#   4. Production call sites do not regress to runtime_async helper shims (formerly named runtime_compat under ft-g43fq, alias removed in ft-y378j.4)
#   5. RCH preflight uses an actual remote-only smoke command and rejects local fallback
#   6. Guard test passes through rch-offloaded cargo execution only
#   7. Smoke test passes through rch-offloaded cargo execution only
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/ft_e34d9_10_5_4_runtime_surface_guard"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_4_runtime_surface_guard"
CORRELATION_ID="ft-e34d9.10.5.4.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary_${RUN_ID}.json"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-e34d9-10-5-4-runtime-surface-guard-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

PASS=0
FAIL=0
TOTAL=0
LAST_STEP_LOG=""

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "e34d9_10_5_4_runtime_surface_guard"

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
    --arg component "runtime_surface_guard.e2e" \
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

record_result() {
  local name="$1"
  local ok="$2"
  TOTAL=$((TOTAL + 1))
  if [ "${ok}" = "true" ]; then
    PASS=$((PASS + 1))
    emit_log "passed" "${name}" "scenario_end" "completed" "none" "${LOG_FILE}" ""
    echo "  PASS: ${name}"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "${name}" "scenario_end" "${3:-assertion_failed}" "${4:-assertion_failed}" "${LOG_FILE}" "${5:-}"
    echo "  FAIL: ${name}"
  fi
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "failed" "preflight" "prereq_check" "missing_prerequisite" "E2E-PREREQ" "${cmd}" "missing:${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

allowed_tokio_runtime_file() {
  local path="$1"
  case "${path}" in
    "crates/frankenterm-core/src/runtime_async.rs")
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_comment_only() {
  local line="$1"
  local trimmed="${line#"${line%%[![:space:]]*}"}"
  [[ "${trimmed}" == //* || "${trimmed}" == '/*'* || "${trimmed}" == '*'* || "${trimmed}" == '*/'* ]]
}

validate_tokio_runtime_allowlist() {
  local mode="$1"
  local output_file="$2"
  local search_dirs=(
    "${ROOT_DIR}/crates/frankenterm-core/src"
    "${ROOT_DIR}/crates/frankenterm/src"
  )
  local raw
  local filtered=""
  local line
  local path
  local rel_path
  local rest
  local source_line

  : > "${output_file}"
  raw="$(rg -n 'tokio::select!|tokio::signal::|tokio::time::sleep|tokio::time::timeout|tokio::sync::mpsc|tokio::sync::watch' "${search_dirs[@]}" -g '*.rs' || true)"

  while IFS= read -r line; do
    [ -z "${line}" ] && continue
    path="${line%%:*}"
    rest="${line#*:}"
    rest="${rest#*:}"
    source_line="${rest}"
    if is_comment_only "${source_line}"; then
      continue
    fi

    rel_path="${path#"${ROOT_DIR}/"}"
    if [[ "${mode}" == "nominal" ]]; then
      if ! allowed_tokio_runtime_file "${rel_path}"; then
        filtered+="${rel_path}:${line#"${path}":}"$'\n'
      fi
    else
      filtered+="${rel_path}:${line#"${path}":}"$'\n'
    fi
  done <<< "${raw}"

  printf "%s" "${filtered}" > "${output_file}"

  if [[ "${mode}" == "nominal" ]]; then
    [ -z "${filtered}" ]
    return
  fi

  [ -s "${output_file}" ]
}

# Validates that the runtime_async helper functions (mpsc_send,
# mpsc_recv_option, etc.) are NOT called outside of runtime_async.rs
# itself. The function was historically named after the legacy
# runtime_compat module (renamed under ft-g43fq, alias removed under
# ft-y378j.4) but the actual check has always been about the canonical
# runtime_async surface contract. Renamed for accuracy.
validate_runtime_async_helper_callsites() {
  local output_file="$1"
  rg -n '\b(mpsc_send|mpsc_recv_option|watch_has_changed|watch_borrow_and_update_clone|watch_changed)\(' \
    "${ROOT_DIR}/crates/frankenterm-core/src" \
    "${ROOT_DIR}/crates/frankenterm/src" \
    -g '*.rs' \
    > "${output_file}" || true
  grep -v '^'"${ROOT_DIR}"'/crates/frankenterm-core/src/runtime_async.rs:' "${output_file}" > "${output_file}.filtered" || true
  mv "${output_file}.filtered" "${output_file}"
  [[ ! -s "${output_file}" ]]
}

run_rch_test_step() {
  local label="$1"
  local test_name="$2"
  shift 2
  local rc

  emit_log "running" "${label}" "rch_test" "started" "none" "${LAST_STEP_LOG}" "${test_name}"
  LAST_STEP_LOG="${ARTIFACT_DIR}/${RUN_ID}_${label}.log"
  set +e
  run_rch_cargo_logged "${LAST_STEP_LOG}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "$@"
  rc=$?
  set -e

  if [[ -f "${LAST_STEP_LOG}" ]]; then
    cat "${LAST_STEP_LOG}"
  fi

  if [[ "${rc}" -eq 0 ]]; then
    record_result "${label}" "true"
  else
    record_result "${label}" "false" "cargo_test_failed" "CARGO-TEST-FAIL" "${test_name}"
  fi
}

run_shared_rch_preflight() {
  local rc=0

  set +e
  ( ensure_rch_ready )
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    emit_log "passed" "rch_probe" "rch_preflight" "rch_workers_probe_ok" "none" "$(rch_probe_log_path)" "workers_probe"
    emit_log "passed" "rch_remote_smoke" "rch_preflight" "rch_remote_smoke_ok" "none" "$(rch_smoke_log_path)" "cargo check --help"
    return 0
  fi

  emit_log "failed" "rch_preflight" "rch_preflight" "rch_preflight_failed" "RCH-E100" "$(rch_smoke_log_path)" "ensure_rch_ready"
  return "${rc}"
}

echo "=== Runtime Surface Guard E2E (ft-e34d9.10.5.4.1) ==="
emit_log "started" "e2e_suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

require_cmd jq
require_cmd rg
require_cmd rch

echo ""
echo "--- Scenario 1: nominal tokio runtime allowlist ---"
TOKIO_NOMINAL_LOG="${ARTIFACT_DIR}/tokio_runtime_nominal_${RUN_ID}.log"
if validate_tokio_runtime_allowlist "nominal" "${TOKIO_NOMINAL_LOG}"; then
  record_result "tokio_runtime_allowlist_nominal" "true"
else
  record_result "tokio_runtime_allowlist_nominal" "false" "tokio_runtime_primitive_leak" "SURFACE-E300" "see $(basename "${TOKIO_NOMINAL_LOG}")"
fi

echo ""
echo "--- Scenario 2: failure injection proves detector sensitivity ---"
TOKIO_FAILURE_LOG="${ARTIFACT_DIR}/tokio_runtime_failure_injection_${RUN_ID}.log"
if validate_tokio_runtime_allowlist "failure_injection" "${TOKIO_FAILURE_LOG}"; then
  record_result "tokio_runtime_allowlist_failure_injection" "true"
else
  record_result "tokio_runtime_allowlist_failure_injection" "false" "detector_missed_expected_failure" "SURFACE-E301" "see $(basename "${TOKIO_FAILURE_LOG}")"
fi

echo ""
echo "--- Scenario 3: recovery restores nominal allowlist ---"
TOKIO_RECOVERY_LOG="${ARTIFACT_DIR}/tokio_runtime_recovery_${RUN_ID}.log"
if validate_tokio_runtime_allowlist "nominal" "${TOKIO_RECOVERY_LOG}"; then
  record_result "tokio_runtime_allowlist_recovery" "true"
else
  record_result "tokio_runtime_allowlist_recovery" "false" "recovery_check_failed" "SURFACE-E302" "see $(basename "${TOKIO_RECOVERY_LOG}")"
fi

echo ""
echo "--- Scenario 4: runtime_async helper shims stay out of production call sites ---"
HELPER_LOG="${ARTIFACT_DIR}/runtime_async_helper_callsites_${RUN_ID}.log"
if validate_runtime_async_helper_callsites "${HELPER_LOG}"; then
  record_result "runtime_async_helper_callsites" "true"
else
  record_result "runtime_async_helper_callsites" "false" "unexpected_helper_callsite" "SURFACE-E303" "see $(basename "${HELPER_LOG}")"
fi

echo ""
echo "--- Scenario 5: rch remote-only smoke preflight ---"
if ! run_shared_rch_preflight; then
  echo "rch remote smoke preflight failed; refusing local fallback" >&2
  exit 2
fi

echo ""
echo "--- Scenario 6: runtime_async_surface_guard passes via rch ---"
run_rch_test_step \
  "runtime_async_surface_guard" \
  "cargo test -p frankenterm-core --test runtime_async_surface_guard -- --nocapture" \
  cargo test -p frankenterm-core --test runtime_async_surface_guard -- --nocapture

echo ""
echo "--- Scenario 7: runtime_async_smoke passes via rch ---"
run_rch_test_step \
  "runtime_async_smoke" \
  "cargo test -p frankenterm-core --test runtime_async_smoke -- --nocapture" \
  cargo test -p frankenterm-core --test runtime_async_smoke -- --nocapture

echo ""
echo "=== Summary ==="
echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
echo "  Log: ${LOG_FILE}"

emit_log "$([ "${FAIL}" -eq 0 ] && echo passed || echo failed)" \
  "e2e_suite" "script_end" "completed" "none" "${LOG_FILE}" \
  "total=${TOTAL},pass=${PASS},fail=${FAIL}"

jq -cn \
  --arg test "${SCENARIO_ID}" \
  --arg run_id "${RUN_ID}" \
  --arg log_file "${LOG_FILE}" \
  --argjson pass "${PASS}" \
  --argjson fail "${FAIL}" \
  --argjson total "${TOTAL}" \
  '{
    test: $test,
    run_id: $run_id,
    scenarios_pass: $pass,
    scenarios_fail: $fail,
    total: $total,
    log_file: $log_file
  }' > "${SUMMARY_FILE}"

[ "${FAIL}" -eq 0 ]
