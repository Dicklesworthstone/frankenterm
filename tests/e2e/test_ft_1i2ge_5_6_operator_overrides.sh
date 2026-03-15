#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.5.6 — Operator override controls (pin/exclude/reprioritize)
# E2E scenario: validate that operator overrides integrate correctly with
# the mission planner pipeline, including failure injection and recovery.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_5_6_operator_overrides"
CORRELATION_ID="ft-1i2ge.5.6-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_5_6_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"

RCH_TARGET_DIR="target/rch-e2e-operator-overrides-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/operator_overrides_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/operator_overrides_${RUN_ID}.smoke.log"

fatal() { echo "FATAL: $1" >&2; exit 1; }

run_rch() {
    TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
    run_rch exec -- env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
    grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback() {
    local output_file="$1"
    if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${output_file}" 2>/dev/null; then
        fatal "rch fell back to local execution; refusing offload policy violation. See ${output_file}"
    fi
}

run_rch_cargo_logged() {
    local output_file="$1"
    shift
    set +e
    (
        cd "${ROOT_DIR}"
        run_rch_cargo "$@"
    ) >"${output_file}" 2>&1
    local rc=$?
    set -e
    check_rch_fallback "${output_file}"
    return "${rc}"
}

ensure_rch_ready() {
    if ! command -v rch >/dev/null 2>&1; then
        fatal "rch is required for this e2e harness; refusing local cargo execution."
    fi
    set +e
    run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
    local probe_rc=$?
    set -e
    if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
        fatal "rch workers are unavailable; refusing local cargo execution. See ${RCH_PROBE_LOG}"
    fi
    set +e
    run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
    local smoke_rc=$?
    set -e
    check_rch_fallback "${RCH_SMOKE_LOG}"
    if [[ ${smoke_rc} -ne 0 ]]; then
        fatal "rch remote smoke preflight failed; refusing local cargo execution. See ${RCH_SMOKE_LOG}"
    fi
}

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
    --arg component "mission_operator_overrides.e2e" \
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

emit_log "started" "script_init" "none" "none" \
  "$(basename "${LOG_FILE}")" \
  "operator override controls e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

ensure_rch_ready

# ── Test 1: Compile check with subprocess-bridge feature ─────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check with subprocess-bridge feature"

compile_log="${LOG_DIR}/ft_1i2ge_5_6_${RUN_ID}.compile.log"
set +e
run_rch_cargo_logged "${compile_log}" \
  check -p frankenterm-core --features subprocess-bridge
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_5_6_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Override unit tests pass ─────────────────────────────────
emit_log "running" "unit_tests" "cargo_test" "none" \
  "none" "run override unit tests"

tests_log="${LOG_DIR}/ft_1i2ge_5_6_${RUN_ID}.tests.log"
set +e
run_rch_cargo_logged "${tests_log}" \
  test -p frankenterm-core --features subprocess-bridge --lib \
  -- "mission_loop::tests"
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "unit_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_5_6_${RUN_ID}.tests.log" "override unit tests failed"
  echo "FAIL: unit tests" >&2
  exit 1
fi

# Count passing override tests.
override_count=$(grep -c "override.*ok$" "${tests_log}" || echo 0)
evaluate_count=$(grep -c "evaluate_with.*ok$" "${tests_log}" || echo 0)
total_override=$((override_count + evaluate_count))

if [[ ${total_override} -lt 17 ]]; then
  emit_log "failed" "unit_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_5_6_${RUN_ID}.tests.log" \
    "only ${total_override} override tests passed (expected >=17)"
  echo "FAIL: insufficient override test coverage (${total_override} < 17)" >&2
  exit 1
fi
emit_log "passed" "unit_tests" "all_tests_ok" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.tests.log" \
  "${total_override} override tests + all mission_loop tests passed"

# ── Test 3: Clippy clean ─────────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in mission_loop"

clippy_log="${LOG_DIR}/ft_1i2ge_5_6_${RUN_ID}.clippy.log"
set +e
run_rch_cargo_logged "${clippy_log}" \
  clippy -p frankenterm-core --features subprocess-bridge --lib --tests
clippy_rc=$?
set -e

mission_loop_warnings=$(grep -c "mission_loop.rs" "${clippy_log}" || echo 0)
if [[ ${mission_loop_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_5_6_${RUN_ID}.clippy.log" \
    "${mission_loop_warnings} clippy warnings in mission_loop.rs"
  echo "FAIL: clippy warnings in mission_loop.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.clippy.log" "zero clippy warnings in mission_loop.rs"

# ── Test 4: Override types serde contract ─────────────────────────────
emit_log "running" "serde_contract" "type_contract" "none" \
  "none" "validate override types serialize/deserialize correctly"

# Ensure the serde roundtrip test is among passing tests.
if ! grep -q "override_state_serde_roundtrip.*ok" "${tests_log}"; then
  emit_log "failed" "serde_contract" "missing_serde_test" "CONTRACT_MISSING" \
    "ft_1i2ge_5_6_${RUN_ID}.tests.log" "override_state_serde_roundtrip test not found"
  echo "FAIL: serde roundtrip test missing" >&2
  exit 1
fi
emit_log "passed" "serde_contract" "serde_verified" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.tests.log" "override serde roundtrip verified"

# ── Test 5: Determinism check (repeat run) ───────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify test results are deterministic across repeated runs"

repeat_log="${LOG_DIR}/ft_1i2ge_5_6_${RUN_ID}.tests_repeat.log"
set +e
run_rch_cargo_logged "${repeat_log}" \
  test -p frankenterm-core --features subprocess-bridge --lib \
  -- "mission_loop::tests"
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_5_6_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

# Compare test counts between runs.
pass_count_1=$(grep -c "ok$" "${tests_log}" || echo 0)
pass_count_2=$(grep -c "ok$" "${repeat_log}" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_5_6_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Test 6: Recovery path — clear all overrides ──────────────────────
emit_log "running" "recovery_path" "override_clear" "none" \
  "none" "validate that clearing overrides restores normal pipeline behavior"

# The clear_override_moves_to_history test validates this path.
if ! grep -q "clear_override_moves_to_history.*ok" "${tests_log}"; then
  emit_log "failed" "recovery_path" "missing_clear_test" "RECOVERY_MISSING" \
    "ft_1i2ge_5_6_${RUN_ID}.tests.log" "clear_override recovery test not found"
  echo "FAIL: recovery path test missing" >&2
  exit 1
fi
emit_log "passed" "recovery_path" "clear_verified" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.tests.log" "override clear/recovery path verified"

# ── Test 7: Failure injection — expired override eviction ────────────
emit_log "running" "failure_injection" "ttl_expiry" "none" \
  "none" "validate that expired overrides are correctly evicted"

if ! grep -q "evaluate_with_expired_override_evicted.*ok" "${tests_log}"; then
  emit_log "failed" "failure_injection" "missing_expiry_test" "INJECTION_MISSING" \
    "ft_1i2ge_5_6_${RUN_ID}.tests.log" "expired override eviction test not found"
  echo "FAIL: failure injection test missing" >&2
  exit 1
fi
emit_log "passed" "failure_injection" "expiry_verified" "none" \
  "ft_1i2ge_5_6_${RUN_ID}.tests.log" "expired override eviction verified"

# ── Suite complete ───────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated override types, pipeline integration, serde contract, determinism, recovery, and failure injection"

echo "ft-1i2ge.5.6 e2e passed. Logs: ${LOG_FILE_REL}"
