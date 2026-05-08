#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.7.5 — Production go/no-go decision package
# E2E scenario: validate go/no-go tests compile, pass, are clippy-clean,
# cover all decision-package categories, and produce deterministic results.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_7_5_go_no_go"
CORRELATION_ID="ft-1i2ge.7.5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_7_5_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

RCH_TARGET_DIR="target/rch-e2e-go-no-go-${RUN_ID}"
GUARD_LIB="$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

run_cargo_step() {
  local output_file="$1"
  shift
  run_rch_cargo_logged "${output_file}" env CARGO_TARGET_DIR="${RCH_TARGET_DIR}" cargo "$@"
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
    --arg component "go_no_go.e2e" \
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

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${GUARD_LIB}"
rch_init "${LOG_DIR}" "${RUN_ID}" "1i2ge_7_5_go_no_go"
ensure_rch_ready

emit_log "started" "script_init" "none" "none" \
  "$(basename "${LOG_FILE}")" \
  "go/no-go e2e started"

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check go/no-go tests"

compile_log="${LOG_DIR}/ft_1i2ge_7_5_${RUN_ID}.compile.log"
set +e
run_cargo_step "${compile_log}" check -p frankenterm-core --features subprocess-bridge \
  --test mission_go_no_go
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_7_5_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_7_5_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Go/no-go tests pass ───────────────────────────────────
emit_log "running" "go_no_go_tests" "cargo_test" "none" \
  "none" "run go/no-go tests"

tests_log="${LOG_DIR}/ft_1i2ge_7_5_${RUN_ID}.tests.log"
set +e
run_cargo_step "${tests_log}" test -p frankenterm-core --features subprocess-bridge \
  --test mission_go_no_go
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "go_no_go_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_7_5_${RUN_ID}.tests.log" "go/no-go tests failed"
  echo "FAIL: go/no-go tests" >&2
  exit 1
fi

gonogo_count=$(count_matches "ok$" "${tests_log}")

if [[ ${gonogo_count} -lt 20 ]]; then
  emit_log "failed" "go_no_go_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_7_5_${RUN_ID}.tests.log" \
    "only ${gonogo_count} go/no-go tests passed (expected >=20)"
  echo "FAIL: insufficient go/no-go test coverage (${gonogo_count} < 20)" >&2
  exit 1
fi
emit_log "passed" "go_no_go_tests" "all_tests_ok" "none" \
  "ft_1i2ge_7_5_${RUN_ID}.tests.log" \
  "${gonogo_count} go/no-go tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in go/no-go tests"

clippy_log="${LOG_DIR}/ft_1i2ge_7_5_${RUN_ID}.clippy.log"
set +e
run_cargo_step "${clippy_log}" clippy -p frankenterm-core --features subprocess-bridge \
  --test mission_go_no_go
clippy_rc=$?
set -e

if [[ ${clippy_rc} -ne 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_failed" "CLIPPY_FAIL" \
    "$(basename "${clippy_log}")" "cargo clippy failed"
  echo "FAIL: cargo clippy failed" >&2
  exit 1
fi

gonogo_warnings=$(count_matches "mission_go_no_go.rs" "${clippy_log}")
if [[ ${gonogo_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_7_5_${RUN_ID}.clippy.log" \
    "${gonogo_warnings} clippy warnings in mission_go_no_go.rs"
  echo "FAIL: clippy warnings in mission_go_no_go.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_7_5_${RUN_ID}.clippy.log" "zero clippy warnings"

# ── Test 4: Category coverage ─────────────────────────────────────
emit_log "running" "category_coverage" "coverage_check" "none" \
  "none" "validate all go/no-go categories covered"

missing_categories=0

for pattern in \
  "readiness_" \
  "evidence_" \
  "threshold_" \
  "rollback_" \
  "rubric_" \
  "go_no_go_" \
  "dedup_" \
  "report_" \
  "determinism_"; do
  if ! grep -q "${pattern}.*ok" "${tests_log}"; then
    echo "MISSING: ${pattern}" >&2
    missing_categories=$((missing_categories + 1))
  fi
done

if [[ ${missing_categories} -gt 0 ]]; then
  emit_log "failed" "category_coverage" "missing_categories" "COVERAGE_MISSING" \
    "ft_1i2ge_7_5_${RUN_ID}.tests.log" \
    "${missing_categories} go/no-go categories missing"
  echo "FAIL: ${missing_categories} go/no-go categories missing" >&2
  exit 1
fi
emit_log "passed" "category_coverage" "all_categories_covered" "none" \
  "ft_1i2ge_7_5_${RUN_ID}.tests.log" "all go/no-go categories covered"

# ── Test 5: Determinism check ──────────────────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify go/no-go results are deterministic"

repeat_log="${LOG_DIR}/ft_1i2ge_7_5_${RUN_ID}.tests_repeat.log"
set +e
run_cargo_step "${repeat_log}" test -p frankenterm-core --features subprocess-bridge \
  --test mission_go_no_go
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_7_5_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(count_matches "ok$" "${tests_log}")
pass_count_2=$(count_matches "ok$" "${repeat_log}")
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_7_5_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_7_5_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated go/no-go: compilation, ${gonogo_count} tests, clippy, category coverage, determinism"

echo "ft-1i2ge.7.5 e2e passed. Logs: ${LOG_FILE_REL}"
