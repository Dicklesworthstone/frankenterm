#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/ft_e34d9_10_5_3_ssh_transport"
mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_5_3_ssh_transport"
CORRELATION_ID="ft-e34d9.10.5.3-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary_${RUN_ID}.json"
RCH_REMOTE_TMPDIR="${RCH_REMOTE_TMPDIR:-/var/tmp}"
RCH_TARGET_DIR="${RCH_REMOTE_TMPDIR}/rch-e2e-ft-e34d9-10-5-3-ssh-transport-${RUN_ID}"
# The shared cargo-check smoke preflight currently hits a known post-success
# artifact retrieval stall in this checkout. We still fail closed via worker
# probe + the actual rch-backed SSH phases below.
RCH_SKIP_SMOKE_PREFLIGHT="${RCH_SKIP_SMOKE_PREFLIGHT:-1}"

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${ARTIFACT_DIR}" "${RUN_ID}" "${SCENARIO_ID}" "${ROOT_DIR}"
# In this checkout, rch commonly logs "Dependency planner fail-open" while still
# executing the command remotely with primary-root-only sync. Treat genuine
# local fallback as off-policy, but allow this repo-specific remote-sync warning.
# shellcheck disable=SC2034
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'

PASS_COUNT=0
FAIL_COUNT=0
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
    --arg component "ssh_transport.e2e" \
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

  if run_rch_cargo_logged "${output_file}" env TMPDIR="${RCH_REMOTE_TMPDIR}" CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"; then
    passed_count=$(grep -c '\.\.\. ok' "${output_file}" || true)
    if [[ "${passed_count}" -eq 0 ]]; then
      passed_count=1
    fi
    PASS_COUNT=$((PASS_COUNT + passed_count))
    emit_log "pass" "${phase}" "${phase}_complete" "all_tests_passed" "none" "${output_file}" "${target_desc}"
    echo "  PASS: ${phase} (${passed_count} tests passed)"
  else
    record_failure_count "${output_file}"
    failed_count="${LAST_FAILURE_COUNT}"
    emit_log "fail" "${phase}" "${phase}_complete" "cargo_test_failed" "CARGO-TEST-FAIL" "${output_file}" "${target_desc}"
    echo "  FAIL: ${phase} (${failed_count} failures)"
  fi
}

assert_output_contains() {
  local scenario="$1"
  local file="$2"
  local pattern="$3"
  local artifact_path="$4"

  if grep -Fq "${pattern}" "${file}"; then
    record_structural_pass "${scenario}" "present" "${artifact_path}" "${pattern}"
  else
    record_structural_fail "${scenario}" "missing" "E_OUTPUT" "${artifact_path}" "${pattern}"
  fi
}

require_cmd jq
require_cmd rg

EXEC_FILE="${ROOT_DIR}/frankenterm/ssh/tests/e2e/exec.rs"
MOD_FILE="${ROOT_DIR}/frankenterm/ssh/tests/e2e/mod.rs"
STRUCTURAL_FILE="${ARTIFACT_DIR}/structural_${RUN_ID}.txt"

{
  echo "exec_file=${EXEC_FILE}"
  echo "mod_file=${MOD_FILE}"
} > "${STRUCTURAL_FILE}"

echo "=== SSH transport validation (ft-e34d9.10.5.3) ==="
echo "Run ID:         ${RUN_ID}"
echo "Evidence log:   ${LOG_FILE}"
echo "Artifact dir:   ${ARTIFACT_DIR}"
echo ""

echo "=== Phase 0: Structural expectations ==="
if [[ -f "${EXEC_FILE}" ]]; then
  record_structural_pass "exec_e2e_file" "exists" "${STRUCTURAL_FILE}"
else
  record_structural_fail "exec_e2e_file" "missing" "E_FILE" "${STRUCTURAL_FILE}"
fi

if grep -Fq 'mod exec;' "${MOD_FILE}"; then
  record_structural_pass "exec_module_wired" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "exec_module_wired" "missing" "E_MOD" "${STRUCTURAL_FILE}"
fi

EXEC_TEST_COUNT=$(grep -c '^fn exec_should_' "${EXEC_FILE}" || true)
if [[ "${EXEC_TEST_COUNT}" -ge 4 ]]; then
  record_structural_pass "exec_test_floor" "sufficient" "${STRUCTURAL_FILE}" "count=${EXEC_TEST_COUNT}"
else
  record_structural_fail "exec_test_floor" "insufficient" "E_TESTS" "${STRUCTURAL_FILE}" "count=${EXEC_TEST_COUNT}"
fi

if grep -Fq 'fn wait_for_exit' "${EXEC_FILE}" && grep -Fq 'child.kill().expect("kill should signal the remote exec")' "${EXEC_FILE}"; then
  record_structural_pass "kill_recovery_path_present" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "kill_recovery_path_present" "missing" "E_KILL" "${STRUCTURAL_FILE}"
fi

if grep -Fq 'frankenterm_ssh_e2e_exec' "${EXEC_FILE}" \
  && grep -Fq 'scenario_id' "${EXEC_FILE}" \
  && grep -Fq 'command' "${EXEC_FILE}" \
  && grep -Fq 'reason_code' "${EXEC_FILE}" \
  && grep -Fq 'exit_code' "${EXEC_FILE}"; then
  record_structural_pass "structured_log_fields" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "structured_log_fields" "missing" "E_LOG_FIELDS" "${STRUCTURAL_FILE}"
fi

if grep -Fq 'exec-stdin-path' "${EXEC_FILE}" \
  && grep -Fq 'exec-stdin-recovery-path' "${EXEC_FILE}" \
  && grep -Fq 'collect_exec_result_with_input' "${EXEC_FILE}"; then
  record_structural_pass "stdin_recovery_path_present" "present" "${STRUCTURAL_FILE}"
else
  record_structural_fail "stdin_recovery_path_present" "missing" "E_STDIN" "${STRUCTURAL_FILE}"
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
echo "=== Phase 2: Focused SSH exec scenarios ==="
run_rch_phase \
  "exec_happy_path" \
  "cargo test -p frankenterm-ssh --test lib exec_should_capture_stdout_stderr_and_zero_exit -- --nocapture" \
  test -p frankenterm-ssh --test lib exec_should_capture_stdout_stderr_and_zero_exit -- --nocapture
assert_output_contains "exec_happy_path_log" "${ARTIFACT_DIR}/exec_happy_path_${RUN_ID}.log" '"scenario_id":"exec-happy-path"' "${ARTIFACT_DIR}/exec_happy_path_${RUN_ID}.log"

run_rch_phase \
  "exec_failure_recovery" \
  "cargo test -p frankenterm-ssh --test lib exec_should_recover_after_non_zero_exit_on_same_session -- --nocapture" \
  test -p frankenterm-ssh --test lib exec_should_recover_after_non_zero_exit_on_same_session -- --nocapture
assert_output_contains "exec_failure_log" "${ARTIFACT_DIR}/exec_failure_recovery_${RUN_ID}.log" '"scenario_id":"exec-failure-path"' "${ARTIFACT_DIR}/exec_failure_recovery_${RUN_ID}.log"
assert_output_contains "exec_recovery_log" "${ARTIFACT_DIR}/exec_failure_recovery_${RUN_ID}.log" '"scenario_id":"exec-recovery-path"' "${ARTIFACT_DIR}/exec_failure_recovery_${RUN_ID}.log"

run_rch_phase \
  "exec_stdin_recovery" \
  "cargo test -p frankenterm-ssh --test lib exec_should_echo_stdin_and_allow_followup_exec -- --nocapture" \
  test -p frankenterm-ssh --test lib exec_should_echo_stdin_and_allow_followup_exec -- --nocapture
assert_output_contains "exec_stdin_log" "${ARTIFACT_DIR}/exec_stdin_recovery_${RUN_ID}.log" '"scenario_id":"exec-stdin-path"' "${ARTIFACT_DIR}/exec_stdin_recovery_${RUN_ID}.log"
assert_output_contains "exec_stdin_recovery_log" "${ARTIFACT_DIR}/exec_stdin_recovery_${RUN_ID}.log" '"scenario_id":"exec-stdin-recovery-path"' "${ARTIFACT_DIR}/exec_stdin_recovery_${RUN_ID}.log"

run_rch_phase \
  "exec_kill_recovery" \
  "cargo test -p frankenterm-ssh --test lib exec_should_terminate_after_kill_and_allow_followup_exec -- --nocapture" \
  test -p frankenterm-ssh --test lib exec_should_terminate_after_kill_and_allow_followup_exec -- --nocapture
assert_output_contains "exec_kill_log" "${ARTIFACT_DIR}/exec_kill_recovery_${RUN_ID}.log" '"scenario_id":"exec-kill-path"' "${ARTIFACT_DIR}/exec_kill_recovery_${RUN_ID}.log"
assert_output_contains "exec_kill_recovery_log" "${ARTIFACT_DIR}/exec_kill_recovery_${RUN_ID}.log" '"scenario_id":"exec-kill-recovery-path"' "${ARTIFACT_DIR}/exec_kill_recovery_${RUN_ID}.log"

echo ""
echo "=== Phase 3: Determinism rerun ==="
run_rch_phase \
  "exec_suite_rerun" \
  "cargo test -p frankenterm-ssh --test lib exec_should -- --nocapture" \
  test -p frankenterm-ssh --test lib exec_should -- --nocapture
assert_output_contains "exec_suite_rerun_stdin" "${ARTIFACT_DIR}/exec_suite_rerun_${RUN_ID}.log" '"scenario_id":"exec-stdin-path"' "${ARTIFACT_DIR}/exec_suite_rerun_${RUN_ID}.log"
assert_output_contains "exec_suite_rerun_kill" "${ARTIFACT_DIR}/exec_suite_rerun_${RUN_ID}.log" '"scenario_id":"exec-kill-path"' "${ARTIFACT_DIR}/exec_suite_rerun_${RUN_ID}.log"

echo ""
echo "=== Phase 4: Remote lint gate ==="
run_rch_phase \
  "exec_suite_clippy" \
  "cargo clippy -p frankenterm-ssh --test lib -- -D warnings" \
  clippy -p frankenterm-ssh --test lib -- -D warnings

echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
echo "=== Summary ==="
echo "  Total: ${TOTAL} | Pass: ${PASS_COUNT} | Fail: ${FAIL_COUNT}"
echo "  Evidence log: ${LOG_FILE}"
echo "  Correlation ID: ${CORRELATION_ID}"

emit_log "$([ "${FAIL_COUNT}" -eq 0 ] && echo 'pass' || echo 'fail')" \
  "summary" "e2e_complete" \
  "total=${TOTAL},pass=${PASS_COUNT},fail=${FAIL_COUNT}" \
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
  --argjson total "${TOTAL}" \
  '{
    test: $test,
    run_id: $run_id,
    correlation_id: $correlation_id,
    pass: $pass,
    fail: $fail,
    total: $total,
    log_file: $log_file,
    artifact_dir: $artifact_dir
  }' > "${SUMMARY_FILE}"

if [[ "${FAIL_COUNT}" -gt 0 ]]; then
  exit 1
fi
