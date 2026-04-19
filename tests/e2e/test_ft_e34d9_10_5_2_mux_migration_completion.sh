#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# E2E: Mux migration completion validation (ft-e34d9.10.5.2)
#
# Upgrades the older static grep harness into a fail-closed rch-backed
# validation lane with structured evidence artifacts.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/ft_e34d9_10_5_2_mux_migration_completion"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_2_mux_migration_completion"
CORRELATION_ID="ft-e34d9.10.5.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary_${RUN_ID}.json"
RCH_REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-/var/tmp}"
RCH_TARGET_DIR="${RCH_REMOTE_TMPDIR}/rch-target-ft-e34d9-10-5-2-mux-migration-${RUN_ID}"

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e34d9_10_5_2_mux_migration_completion" "${ROOT_DIR}"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
LAST_FAILURE_COUNT=0

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="${5:-none}"
  local artifact_path="${6:-none}"
  local input_summary="${7:-}"
  local ts

  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "mux_migration_completion.e2e" \
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

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "fail" "preflight" "prereq_check" "missing_prerequisite" "E2E-PREREQ" "${LOG_FILE}" "missing:${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

record_structural_pass() {
  local scenario="$1"
  local reason_code="$2"
  local artifact_path="$3"
  local input_summary="${4:-}"
  PASS_COUNT=$((PASS_COUNT + 1))
  emit_log "pass" "${scenario}" "${scenario}_complete" "${reason_code}" "none" "${artifact_path}" "${input_summary}"
  echo "  PASS: ${scenario}"
}

record_structural_fail() {
  local scenario="$1"
  local reason_code="$2"
  local error_code="$3"
  local artifact_path="$4"
  local input_summary="${5:-}"
  FAIL_COUNT=$((FAIL_COUNT + 1))
  emit_log "fail" "${scenario}" "${scenario}_complete" "${reason_code}" "${error_code}" "${artifact_path}" "${input_summary}"
  echo "  FAIL: ${scenario}"
}

record_failure_count() {
  local file="$1"
  local count
  count=$(sed -n 's/.*; \([0-9][0-9]*\) failed;.*/\1/p' "${file}" | tail -n 1)
  if [[ -z "${count}" ]]; then
    count=$(grep -Ec '^test .* \.\.\. FAILED$' "${file}" || true)
  fi
  if [[ "${count}" -eq 0 ]]; then
    count=1
  fi
  LAST_FAILURE_COUNT="${count}"
  FAIL_COUNT=$((FAIL_COUNT + count))
}

run_rch_phase() {
  local phase="$1"
  local target_desc="$2"
  shift 2

  local output_file="${ARTIFACT_DIR}/${phase}_${RUN_ID}.log"
  local passed_count
  local failed_count

  emit_log "start" "${phase}" "${phase}_start" "begin" "none" "${output_file}" "${target_desc}"

  if run_rch_cargo_logged "${output_file}" env TMPDIR="${RCH_REMOTE_TMPDIR}" CARGO_TARGET_DIR="${RCH_TARGET_DIR}" CARGO_BUILD_JOBS=1 cargo "$@"; then
    passed_count=$(grep -c '\.\.\. ok' "${output_file}" || true)
    PASS_COUNT=$((PASS_COUNT + passed_count))
    emit_log "pass" "${phase}" "${phase}_complete" "all_tests_passed" "none" "${output_file}" "passed=${passed_count};target=${target_desc}"
    echo "  PASS: ${phase} (${passed_count} tests passed)"
  else
    record_failure_count "${output_file}"
    failed_count="${LAST_FAILURE_COUNT}"
    emit_log "fail" "${phase}" "${phase}_complete" "cargo_test_failed" "CARGO-TEST-FAIL" "${output_file}" "failed=${failed_count};target=${target_desc}"
    echo "  FAIL: ${phase} (${failed_count} failures)"
  fi
}

require_cmd jq
require_cmd cargo
require_cmd rg

TEST_FILE="${ROOT_DIR}/crates/frankenterm-core/tests/mux_migration_completion.rs"
MUX_POOL_FILE="${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs"
STRUCTURAL_FILE="${ARTIFACT_DIR}/structural_${RUN_ID}.txt"

{
  echo "test_file=${TEST_FILE}"
  echo "mux_pool_file=${MUX_POOL_FILE}"
} > "${STRUCTURAL_FILE}"

echo "=== Mux migration completion validation (ft-e34d9.10.5.2) ==="
echo "Run ID:         ${RUN_ID}"
echo "Evidence log:   ${LOG_FILE}"
echo "Artifact dir:   ${ARTIFACT_DIR}"
echo ""

echo "=== Phase 0: Structural expectations ==="
if [[ -f "${TEST_FILE}" ]]; then
  record_structural_pass "integration_test_file" "exists" "${STRUCTURAL_FILE}"
else
  record_structural_fail "integration_test_file" "missing" "E_FILE" "${STRUCTURAL_FILE}"
fi

if head -10 "${TEST_FILE}" | grep -q 'cfg(all(feature = "asupersync-runtime", feature = "vendored", unix))'; then
  record_structural_pass "feature_gate" "correct" "${STRUCTURAL_FILE}"
else
  record_structural_fail "feature_gate" "missing" "E_GATE" "${STRUCTURAL_FILE}"
fi

TEST_COUNT=$(grep -c '#\[test\]' "${TEST_FILE}" || true)
if [[ "${TEST_COUNT}" -ge 19 ]]; then
  record_structural_pass "integration_test_count" "sufficient" "${STRUCTURAL_FILE}" "count=${TEST_COUNT}"
else
  record_structural_fail "integration_test_count" "insufficient" "E_TESTS" "${STRUCTURAL_FILE}" "count=${TEST_COUNT}"
fi

if rg -q 'simulated_network_read_error_then_recovery_preserves_buffered_data' "${TEST_FILE}"; then
  record_structural_pass "read_recovery_test_presence" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "read_recovery_test_presence" "missing" "E_READ_RECOVERY" "${STRUCTURAL_FILE}"
fi

if rg -q 'pool_timeout_cascade_then_recovery_restores_capacity' "${TEST_FILE}"; then
  record_structural_pass "timeout_recovery_test_presence" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "timeout_recovery_test_presence" "missing" "E_TIMEOUT_RECOVERY" "${STRUCTURAL_FILE}"
fi

if rg -q 'spawn_with_cx_cancelled_pool_acquire_fails_and_healthy_retry_recovers' "${TEST_FILE}"; then
  record_structural_pass "spawn_with_cx_recovery_test_presence" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "spawn_with_cx_recovery_test_presence" "missing" "E_SPAWN_WITH_CX" "${STRUCTURAL_FILE}"
fi

FAULT_REFS=$(rg -c 'SimulatedNetwork|fault|hostile|recovery' "${TEST_FILE}" || true)
if [[ "${FAULT_REFS}" -ge 6 ]]; then
  record_structural_pass "fault_recovery_refs" "present" "${STRUCTURAL_FILE}" "refs=${FAULT_REFS}"
else
  record_structural_fail "fault_recovery_refs" "insufficient" "E_FAULT_RECOVERY" "${STRUCTURAL_FILE}" "refs=${FAULT_REFS}"
fi

MUX_UNIT=$(grep -c '#\[test\]' "${MUX_POOL_FILE}" || true)
if [[ "${MUX_UNIT}" -ge 60 ]]; then
  record_structural_pass "mux_pool_unit_test_floor" "sufficient" "${STRUCTURAL_FILE}" "count=${MUX_UNIT}"
else
  record_structural_fail "mux_pool_unit_test_floor" "insufficient" "E_MUX_UNIT" "${STRUCTURAL_FILE}" "count=${MUX_UNIT}"
fi

echo ""
echo "=== Phase 1: rch remote-only preflight ==="
emit_log "start" "rch_preflight" "rch_preflight_start" "begin" "none" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
if (
  ensure_rch_ready
) >"${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" 2>&1; then
  emit_log "pass" "rch_preflight" "rch_preflight_complete" "rch_ready" "none" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
  echo "  PASS: rch_preflight"
else
  emit_log "fail" "rch_preflight" "rch_preflight_complete" "rch_unavailable_or_fail_open" "RCH-E100" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
  echo "rch preflight failed; refusing local cargo fallback" >&2
  exit 2
fi

echo ""
echo "=== Phase 2: Focused remote recovery-path validation ==="
run_rch_phase \
  "read_recovery_targeted" \
  "cargo test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored simulated_network_read_error_then_recovery_preserves_buffered_data -- --test-threads=1" \
  test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored simulated_network_read_error_then_recovery_preserves_buffered_data -- --test-threads=1

run_rch_phase \
  "timeout_recovery_targeted" \
  "cargo test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored pool_timeout_cascade_then_recovery_restores_capacity -- --test-threads=1" \
  test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored pool_timeout_cascade_then_recovery_restores_capacity -- --test-threads=1

run_rch_phase \
  "spawn_with_cx_recovery_targeted" \
  "cargo test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored spawn_with_cx_cancelled_pool_acquire_fails_and_healthy_retry_recovers -- --test-threads=1" \
  test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored spawn_with_cx_cancelled_pool_acquire_fails_and_healthy_retry_recovers -- --test-threads=1

echo ""
echo "=== Phase 3: Full mux migration completion suite ==="
run_rch_phase \
  "full_suite" \
  "cargo test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored -- --test-threads=1" \
  test -p frankenterm-core --test mux_migration_completion --features asupersync-runtime,vendored -- --test-threads=1

echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
echo "=== Summary ==="
echo "  Total: ${TOTAL} | Pass: ${PASS_COUNT} | Fail: ${FAIL_COUNT} | Skip: ${SKIP_COUNT}"
echo "  Evidence log: ${LOG_FILE}"
echo "  Correlation ID: ${CORRELATION_ID}"

emit_log "$([ "${FAIL_COUNT}" -eq 0 ] && echo 'pass' || echo 'fail')" \
  "summary" "e2e_complete" \
  "total=${TOTAL},pass=${PASS_COUNT},fail=${FAIL_COUNT},skip=${SKIP_COUNT}" \
  "$([ "${FAIL_COUNT}" -eq 0 ] && echo 'none' || echo 'test_failure')" \
  "${LOG_FILE}" ""

jq -cn \
  --arg test "${SCENARIO_ID}" \
  --arg run_id "${RUN_ID}" \
  --arg correlation_id "${CORRELATION_ID}" \
  --arg log_file "${LOG_FILE}" \
  --arg artifact_dir "${ARTIFACT_DIR}" \
  --argjson pass "${PASS_COUNT}" \
  --argjson fail "${FAIL_COUNT}" \
  --argjson skip "${SKIP_COUNT}" \
  --argjson total "${TOTAL}" \
  '{
    test: $test,
    run_id: $run_id,
    correlation_id: $correlation_id,
    pass: $pass,
    fail: $fail,
    skip: $skip,
    total: $total,
    log_file: $log_file,
    artifact_dir: $artifact_dir
  }' > "${SUMMARY_FILE}"

if [[ "${FAIL_COUNT}" -gt 0 ]]; then
  echo "  VERDICT: FAIL"
  exit 1
fi

echo "  VERDICT: PASS"
exit 0
