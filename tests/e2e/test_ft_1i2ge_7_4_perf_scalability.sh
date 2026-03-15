#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.7.4 — Performance and scalability budget verification
# E2E scenario: validate perf/scalability tests compile, pass, are clippy-clean,
# cover all budget categories, and produce deterministic results.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_7_4_perf_scalability"
CORRELATION_ID="ft-1i2ge.7.4-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-ft-1i2ge-7-4-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_7_4_perf_scalability"
ensure_rch_ready

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
    --arg component "perf_scalability.e2e" \
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
  "perf scalability e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.smoke.log"

run_rch() {
  TMPDIR=/tmp rch "$@"
}

run_rch_cargo() {
  run_rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
}

probe_has_reachable_workers() {
  grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

check_rch_fallback_in_logs() {
  local decision_path="$1"
  local artifact_path="$2"
  local input_summary="$3"
  if grep -Eq "$RCH_FAIL_OPEN_REGEX" "$artifact_path" 2>/dev/null; then
    emit_log "failed" "${decision_path}" "rch_local_fallback_detected" "RCH-LOCAL-FALLBACK" \
      "$(basename "${artifact_path}")" "${input_summary}"
    echo "rch fell back to local execution during ${decision_path}; refusing offload policy violation." >&2
    exit 3
  fi
}

run_rch_cargo_logged() {
  local decision_path="$1"
  local artifact_path="$2"
  shift 2

  set +e
  (
    cd "${ROOT_DIR}"
    run_rch_cargo "$@"
  ) 2>&1 | tee "${artifact_path}"
  local rc=${PIPESTATUS[0]}
  set -e
  check_rch_fallback_in_logs "${decision_path}" "${artifact_path}" "rch cargo ${*}"
  return "${rc}"
}

if ! command -v rch >/dev/null 2>&1; then
  emit_log "failed" "execution_preflight" "rch_required_missing" "RCH-E001" \
    "$(basename "${LOG_FILE}")" "rch is required for cargo execution in this scenario"
  echo "rch is required for this e2e scenario; refusing local cargo execution." >&2
  exit 1
fi

set +e
run_rch --json workers probe --all >"${RCH_PROBE_LOG}" 2>&1
probe_rc=$?
set -e
if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
  emit_log "failed" "execution_preflight" "rch_workers_unhealthy" "RCH-E100" \
    "$(basename "${RCH_PROBE_LOG}")" "rch workers are unavailable; refusing local cargo execution"
  echo "rch workers are unavailable; refusing local cargo execution." >&2
  exit 1
fi
emit_log "running" "execution_preflight" "rch_workers_healthy" "none" \
  "$(basename "${RCH_PROBE_LOG}")" "rch workers probe reported reachable capacity"

set +e
run_rch_cargo check --help >"${RCH_SMOKE_LOG}" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "execution_preflight" "${RCH_SMOKE_LOG}" "rch remote smoke check (cargo check --help)"
if [[ ${smoke_rc} -ne 0 ]]; then
  emit_log "failed" "execution_preflight" "rch_remote_smoke_failed" "RCH-E101" \
    "$(basename "${RCH_SMOKE_LOG}")" "rch remote smoke failed; refusing local fallback"
  echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
  exit 1
fi
emit_log "running" "execution_preflight" "rch_remote_smoke_passed" "none" \
  "$(basename "${RCH_SMOKE_LOG}")" "verified remote rch exec path before running cargo checks"

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check perf scalability tests"

compile_log="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.compile.log"
if run_rch_cargo_logged "compile_check" "${compile_log}" \
  check -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability; then
  compile_rc=0
else
  compile_rc=$?
fi

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "$(basename "${compile_log}")" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "$(basename "${compile_log}")" "compilation succeeded"

# ── Test 2: Perf scalability tests pass ──────────────────────────
emit_log "running" "perf_tests" "cargo_test" "none" \
  "none" "run perf scalability tests"

tests_log="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.tests.log"
if run_rch_cargo_logged "perf_tests" "${tests_log}" \
  test -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability; then
  test_rc=0
else
  test_rc=$?
fi

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "perf_tests" "test_failure" "TEST_FAIL" \
    "$(basename "${tests_log}")" "perf scalability tests failed"
  echo "FAIL: perf scalability tests" >&2
  exit 1
fi

perf_count=$(grep -c "ok$" "${tests_log}" || echo 0)
if [[ ${perf_count} -lt 20 ]]; then
  emit_log "failed" "perf_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "$(basename "${tests_log}")" \
    "only ${perf_count} perf tests passed (expected >=20)"
  echo "FAIL: insufficient perf test coverage (${perf_count} < 20)" >&2
  exit 1
fi
emit_log "passed" "perf_tests" "all_tests_ok" "none" \
  "$(basename "${tests_log}")" \
  "${perf_count} perf scalability tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in perf scalability tests"

clippy_log="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.clippy.log"
if run_rch_cargo_logged "clippy_check" "${clippy_log}" \
  clippy -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability; then
  clippy_rc=0
else
  clippy_rc=$?
fi

perf_warnings=$(grep -c "mission_perf_scalability.rs" "${clippy_log}" || echo 0)
if [[ ${perf_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "$(basename "${clippy_log}")" \
    "${perf_warnings} clippy warnings in mission_perf_scalability.rs"
  echo "FAIL: clippy warnings in mission_perf_scalability.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "$(basename "${clippy_log}")" "zero clippy warnings"

# ── Test 4: Budget category coverage ─────────────────────────────
emit_log "running" "category_coverage" "coverage_check" "none" \
  "none" "validate all budget categories covered"

missing_categories=0
for pattern in \
  "perf_" \
  "scale_" \
  "budget_" \
  "trigger_" \
  "determinism_"; do
  if ! grep -q "${pattern}.*ok" "${tests_log}"; then
    echo "MISSING: ${pattern}" >&2
    missing_categories=$((missing_categories + 1))
  fi
done

if [[ ${missing_categories} -gt 0 ]]; then
  emit_log "failed" "category_coverage" "missing_categories" "COVERAGE_MISSING" \
    "$(basename "${tests_log}")" \
    "${missing_categories} budget categories missing"
  echo "FAIL: ${missing_categories} budget categories missing" >&2
  exit 1
fi
emit_log "passed" "category_coverage" "all_categories_covered" "none" \
  "$(basename "${tests_log}")" "all budget categories covered"

# ── Test 5: Determinism check ────────────────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify perf scalability results are deterministic"

tests_repeat_log="${LOG_DIR}/ft_1i2ge_7_4_${RUN_ID}.tests_repeat.log"
if run_rch_cargo_logged "determinism" "${tests_repeat_log}" \
  test -p frankenterm-core --features subprocess-bridge --test mission_perf_scalability; then
  repeat_rc=0
else
  repeat_rc=$?
fi

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "$(basename "${tests_repeat_log}")" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(grep -c "ok$" "${tests_log}" || echo 0)
pass_count_2=$(grep -c "ok$" "${tests_repeat_log}" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "$(basename "${tests_repeat_log}")" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "$(basename "${tests_repeat_log}")" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated perf scalability: compilation, ${perf_count} tests, clippy, category coverage, determinism"

echo "ft-1i2ge.7.4 e2e passed. Logs: ${LOG_FILE_REL}"
