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
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-telemetry-integrity-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

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

count_matches() {
  local pattern="$1"
  local file="$2"
  local count
  count=$(grep -c -- "${pattern}" "${file}" 2>/dev/null || true)
  if [[ -z "${count}" ]]; then
    count=0
  fi
  printf '%s\n' "${count}"
}

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_6_7_telemetry_integrity"
ensure_rch_ready

emit_log "started" "script_init" "none" "none" \
  "$(basename "${LOG_FILE}")" \
  "telemetry integrity e2e started"

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check telemetry integrity tests"

compile_log="${LOG_DIR}/ft_1i2ge_6_7_${RUN_ID}.compile.log"
set +e
run_rch_cargo_logged "${compile_log}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
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
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
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

telemetry_count=$(count_matches "ok$" "${tests_log}")

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
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
  clippy -p frankenterm-core --features subprocess-bridge \
  --test mission_telemetry_integrity
clippy_rc=$?
set -e

if [[ ${clippy_rc} -ne 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_failed" "CLIPPY_FAIL" \
    "$(basename "${clippy_log}")" "cargo clippy failed"
  echo "FAIL: cargo clippy failed" >&2
  exit 1
fi

telemetry_warnings=$(count_matches "mission_telemetry_integrity.rs" "${clippy_log}")
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
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo \
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

pass_count_1=$(count_matches "ok$" "${tests_log}")
pass_count_2=$(count_matches "ok$" "${repeat_log}")
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
