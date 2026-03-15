#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.6.7 — Telemetry integrity and observability quality gates
# E2E scenario: validate telemetry integrity tests compile, pass, are clippy-clean,
# cover all observability categories, and produce deterministic results.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_6_7_telemetry_integrity"
CORRELATION_ID="ft-1i2ge.6.7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"

RCH_TARGET_DIR="target/rch-e2e-telemetry-integrity-${RUN_ID}"
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
RCH_PROBE_LOG="${LOG_DIR}/telemetry_integrity_${RUN_ID}.probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/telemetry_integrity_${RUN_ID}.smoke.log"

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
    --arg component "telemetry_integrity.e2e" \
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
  "telemetry integrity e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

ensure_rch_ready

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check telemetry integrity tests"

compile_log="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.compile.log"
set +e
run_rch_cargo_logged "${compile_log}" \
  check -p frankenterm-core --features subprocess-bridge \
  --test mission_telemetry_integrity
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_6_7_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_6_7_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Telemetry integrity tests pass ─────────────────────────
emit_log "running" "telemetry_tests" "cargo_test" "none" \
  "none" "run telemetry integrity tests"

tests_log="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.tests.log"
set +e
run_rch_cargo_logged "${tests_log}" \
  test -p frankenterm-core --features subprocess-bridge \
  --test mission_telemetry_integrity
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "telemetry_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_6_7_${RUN_ID}.tests.log" "telemetry integrity tests failed"
  echo "FAIL: telemetry integrity tests" >&2
  exit 1
fi

telemetry_count=$(grep -c "ok$" "${tests_log}" || echo 0)

if [[ ${telemetry_count} -lt 20 ]]; then
  emit_log "failed" "telemetry_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_6_7_${RUN_ID}.tests.log" \
    "only ${telemetry_count} telemetry tests passed (expected >=20)"
  echo "FAIL: insufficient telemetry test coverage (${telemetry_count} < 20)" >&2
  exit 1
fi
emit_log "passed" "telemetry_tests" "all_tests_ok" "none" \
  "ft_1i2ge_6_7_${RUN_ID}.tests.log" \
  "${telemetry_count} telemetry integrity tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in telemetry integrity tests"

clippy_log="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.clippy.log"
set +e
run_rch_cargo_logged "${clippy_log}" \
  clippy -p frankenterm-core --features subprocess-bridge \
  --test mission_telemetry_integrity
clippy_rc=$?
set -e

telemetry_warnings=$(grep -c "mission_telemetry_integrity.rs" "${clippy_log}" || echo 0)
if [[ ${telemetry_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_6_7_${RUN_ID}.clippy.log" \
    "${telemetry_warnings} clippy warnings in mission_telemetry_integrity.rs"
  echo "FAIL: clippy warnings in mission_telemetry_integrity.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_6_7_${RUN_ID}.clippy.log" "zero clippy warnings"

# ── Test 4: Observability category coverage ────────────────────────
emit_log "running" "category_coverage" "coverage_check" "none" \
  "none" "validate all observability categories covered"

missing_categories=0

for pattern in \
  "taxonomy_" \
  "log_" \
  "query_" \
  "metrics_" \
  "report_" \
  "trust_" \
  "determinism_"; do
  if ! grep -q "${pattern}.*ok" "${tests_log}"; then
    echo "MISSING: ${pattern}" >&2
    missing_categories=$((missing_categories + 1))
  fi
done

if [[ ${missing_categories} -gt 0 ]]; then
  emit_log "failed" "category_coverage" "missing_categories" "COVERAGE_MISSING" \
    "ft_1i2ge_6_7_${RUN_ID}.tests.log" \
    "${missing_categories} observability categories missing"
  echo "FAIL: ${missing_categories} observability categories missing" >&2
  exit 1
fi
emit_log "passed" "category_coverage" "all_categories_covered" "none" \
  "ft_1i2ge_6_7_${RUN_ID}.tests.log" "all observability categories covered"

# ── Test 5: Determinism check ────────────────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify telemetry integrity results are deterministic"

repeat_log="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.tests_repeat.log"
set +e
run_rch_cargo_logged "${repeat_log}" \
  test -p frankenterm-core --features subprocess-bridge \
  --test mission_telemetry_integrity
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_6_7_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(grep -c "ok$" "${tests_log}" || echo 0)
pass_count_2=$(grep -c "ok$" "${repeat_log}" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_6_7_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_6_7_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated telemetry integrity: compilation, ${telemetry_count} tests, clippy, category coverage, determinism"

echo "ft-1i2ge.6.7 e2e passed. Logs: ${LOG_FILE_REL}"
