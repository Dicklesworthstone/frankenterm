#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# E2E: Core↔Vendored Async Contract Boundary (ft-e34d9.10.5.4)
#
# Runs the full contract test suite (structural + behavioral) and produces
# a machine-parseable evidence bundle for audit/triage.
# =============================================================================

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_4_async_contract"
CORRELATION_ID="ft-e34d9.10.5.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/vendored_async_contract_${RUN_ID}.jsonl"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/ft_e34d9_10_5_4_async_contract"
SUMMARY_FILE="${ARTIFACT_DIR}/summary_${RUN_ID}.json"
RCH_REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-/var/tmp}"
RCH_TARGET_DIR="${RCH_REMOTE_TMPDIR}/rch-e2e-ft-e34d9-10-5-4-async-contract-${RUN_ID}"
RCH_SMOKE_TIMEOUT_SECS="${RCH_SMOKE_TIMEOUT_SECS:-600}"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "ft_e34d9_10_5_4_async_contract" "${ROOT_DIR}"

# Structured log emitter
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
    --arg component "vendored_async_contract.e2e" \
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

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
LAST_FAILURE_COUNT=0

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "fail" "preflight" "prereq_check" "missing_prerequisite" "E2E-PREREQ" "${LOG_FILE}" "missing:${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
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
  local test_target="$2"
  shift 2

  local output_file="${ARTIFACT_DIR}/${phase}_${RUN_ID}.log"
  local passed_count
  local failed_count

  emit_log "start" "${phase}" "${phase}_start" "begin" "none" "${output_file}" "${test_target}"

  if run_rch_cargo_logged "${output_file}" env TMPDIR="${RCH_REMOTE_TMPDIR}" CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"; then
    passed_count=$(grep -c '\.\.\. ok' "${output_file}" || true)
    PASS_COUNT=$((PASS_COUNT + passed_count))
    emit_log "pass" "${phase}" "${phase}_complete" "all_tests_passed" "none" "${output_file}" "passed=${passed_count};target=${test_target}"
    echo "  PASS: ${phase} (${passed_count} tests passed)"
  else
    record_failure_count "${output_file}"
    failed_count="${LAST_FAILURE_COUNT}"
    emit_log "fail" "${phase}" "${phase}_complete" "cargo_test_failed" "CARGO-TEST-FAIL" "${output_file}" "failed=${failed_count};target=${test_target}"
    echo "  FAIL: ${phase} (${failed_count} failures)"
  fi
}

require_cmd jq
require_cmd cargo

echo "=== Preflight: rch remote-only execution ==="
emit_log "start" "rch_preflight" "rch_preflight_start" "begin" "none" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
if (
  ensure_rch_ready
) >"${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" 2>&1; then
  emit_log "pass" "rch_preflight" "rch_preflight_complete" "rch_ready" "none" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
else
  emit_log "fail" "rch_preflight" "rch_preflight_complete" "rch_unavailable_or_fail_open" "RCH-E100" "${ARTIFACT_DIR}/rch_preflight_${RUN_ID}.log" "ensure_rch_ready"
  echo "rch preflight failed; refusing local cargo fallback" >&2
  exit 2
fi

# ---- Phase 1: Structural / static analysis tests ----------------------------

echo "=== Phase 1: Structural contract verification ==="
run_rch_phase \
  "structural" \
  "cargo test -p frankenterm-core --test vendored_async_contract_verification --no-default-features -- --test-threads=1" \
  test -p frankenterm-core --test vendored_async_contract_verification --no-default-features -- --test-threads=1

# ---- Phase 2: Behavioral runtime tests --------------------------------------

echo "=== Phase 2: Behavioral contract verification ==="
run_rch_phase \
  "behavioral" \
  "cargo test -p frankenterm-core --test vendored_async_contract_behavioral --no-default-features -- --test-threads=1" \
  test -p frankenterm-core --test vendored_async_contract_behavioral --no-default-features -- --test-threads=1

# ---- Phase 3: Integration / compliance infrastructure -----------------------

echo "=== Phase 3: Contract integration tests ==="
run_rch_phase \
  "integration" \
  "cargo test -p frankenterm-core --test vendored_async_contract_integration --no-default-features -- --test-threads=1" \
  test -p frankenterm-core --test vendored_async_contract_integration --no-default-features -- --test-threads=1

# ---- Phase 4: Vendored feature error-context spot checks --------------------

echo "=== Phase 4: Vendored error-context spot checks ==="
run_rch_phase \
  "vendored_error_context" \
  "cargo test -p frankenterm-core --test vendored_async_contract_behavioral --no-default-features --features vendored vendored_error_chain_preserves -- --test-threads=1" \
  test -p frankenterm-core --test vendored_async_contract_behavioral --no-default-features --features vendored vendored_error_chain_preserves -- --test-threads=1

# ---- Phase 5: Surface guard static analysis ---------------------------------

echo "=== Phase 5: Surface guard confinement tests ==="
run_rch_phase \
  "surface_guard" \
  "cargo test -p frankenterm-core --test runtime_async_surface_guard --no-default-features -- --test-threads=1" \
  test -p frankenterm-core --test runtime_async_surface_guard --no-default-features -- --test-threads=1

# ---- Phase 6: Repeat-run stability (determinism) ----------------------------

echo "=== Phase 6: Repeat-run stability (3 iterations) ==="
emit_log "start" "stability" "phase6_start" "begin" "none" "${ARTIFACT_DIR}/stability_run1_${RUN_ID}.log" "3-pass determinism check"

STABILITY_OK=true
for iteration in 1 2 3; do
  stability_log="${ARTIFACT_DIR}/stability_run${iteration}_${RUN_ID}.log"
  if ! run_rch_cargo_logged "${stability_log}" \
    env TMPDIR="${RCH_REMOTE_TMPDIR}" CARGO_TARGET_DIR="${RCH_TARGET_DIR}" \
    cargo test -p frankenterm-core --test vendored_async_contract_behavioral --no-default-features -- --test-threads=1; then
    STABILITY_OK=false
    emit_log "fail" "stability_run${iteration}" "phase6_iteration" \
      "stability_failure" "non_deterministic" \
      "${stability_log}" "iteration=${iteration}"
    echo "  FAIL: stability run ${iteration} failed"
  fi
done

if [ "${STABILITY_OK}" = true ]; then
  emit_log "pass" "stability" "phase6_complete" "3_iterations_stable" "none" "${ARTIFACT_DIR}" "all_3_passed"
  echo "  PASS: all 3 stability runs passed"
else
  FAIL_COUNT=$((FAIL_COUNT + 1))
  emit_log "fail" "stability" "phase6_complete" "stability_failure" "non_deterministic" "${ARTIFACT_DIR}" ""
  echo "  FAIL: repeat-run stability check failed"
fi

# ---- Summary ----------------------------------------------------------------

TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))
echo ""
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

if [ "${FAIL_COUNT}" -gt 0 ]; then
  echo "  VERDICT: FAIL"
  exit 1
fi

echo "  VERDICT: PASS"
exit 0
