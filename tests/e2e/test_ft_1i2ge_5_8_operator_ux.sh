#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.5.8 — Operator workflow UX validation, friction heatmap, remediation
# E2E scenario: validate operator UX tests compile, pass, are clippy-clean,
# cover all workflow scenarios, and produce deterministic results.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_5_8_operator_ux"
CORRELATION_ID="ft-1i2ge.5.8-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#${ROOT_DIR}/}"

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
    --arg component "operator_workflow_ux.e2e" \
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
  "operator workflow UX e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check operator workflow UX tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-8-${RUN_ID}" \
    cargo check -p frankenterm-core --features subprocess-bridge \
    --test operator_workflow_ux 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.compile.log" 2>&1
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_5_8_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_5_8_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Operator workflow UX tests pass ────────────────────────
emit_log "running" "ux_tests" "cargo_test" "none" \
  "none" "run operator workflow UX tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-8-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test operator_workflow_ux 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests.log" 2>&1
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "ux_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_5_8_${RUN_ID}.tests.log" "operator workflow UX tests failed"
  echo "FAIL: operator workflow UX tests" >&2
  exit 1
fi

ux_count=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests.log" || echo 0)

if [[ ${ux_count} -lt 30 ]]; then
  emit_log "failed" "ux_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_5_8_${RUN_ID}.tests.log" \
    "only ${ux_count} UX tests passed (expected >=30)"
  echo "FAIL: insufficient UX test coverage (${ux_count} < 30)" >&2
  exit 1
fi
emit_log "passed" "ux_tests" "all_tests_ok" "none" \
  "ft_1i2ge_5_8_${RUN_ID}.tests.log" \
  "${ux_count} operator workflow UX tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in operator workflow UX tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-8-${RUN_ID}" \
    cargo clippy -p frankenterm-core --features subprocess-bridge \
    --test operator_workflow_ux 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.clippy.log" 2>&1
clippy_rc=$?
set -e

ux_warnings=$(grep -c "operator_workflow_ux.rs" "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.clippy.log" || echo 0)
if [[ ${ux_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_5_8_${RUN_ID}.clippy.log" \
    "${ux_warnings} clippy warnings in operator_workflow_ux.rs"
  echo "FAIL: clippy warnings in operator_workflow_ux.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_5_8_${RUN_ID}.clippy.log" "zero clippy warnings"

# ── Test 4: Scenario coverage ─────────────────────────────────────
emit_log "running" "scenario_coverage" "coverage_check" "none" \
  "none" "validate all 11 operator workflow scenarios covered"

missing_scenarios=0

for pattern in \
  "routine_monitoring" \
  "incident_" \
  "override_full_lifecycle" \
  "conflict_detection" \
  "safety_envelope" \
  "explainability_" \
  "degradation_" \
  "recovery_" \
  "override_state_accessible" \
  "trigger_batch" \
  "e2e_operator_journey"; do
  if ! grep -q "${pattern}.*ok" "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests.log"; then
    echo "MISSING: ${pattern}" >&2
    missing_scenarios=$((missing_scenarios + 1))
  fi
done

if [[ ${missing_scenarios} -gt 0 ]]; then
  emit_log "failed" "scenario_coverage" "missing_scenarios" "COVERAGE_MISSING" \
    "ft_1i2ge_5_8_${RUN_ID}.tests.log" \
    "${missing_scenarios} workflow scenarios missing"
  echo "FAIL: ${missing_scenarios} workflow scenarios missing" >&2
  exit 1
fi
emit_log "passed" "scenario_coverage" "all_scenarios_covered" "none" \
  "ft_1i2ge_5_8_${RUN_ID}.tests.log" "all workflow scenarios covered"

# ── Test 5: Determinism check ─────────────────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify UX test results are deterministic"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-8-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test operator_workflow_ux 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests_repeat.log" 2>&1
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_5_8_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests.log" || echo 0)
pass_count_2=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_8_${RUN_ID}.tests_repeat.log" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_5_8_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_5_8_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated operator UX: compilation, ${ux_count} tests, clippy, scenario coverage, determinism"

# Cleanup ephemeral target dir.
rm -rf "${ROOT_DIR}/target-e2e-1i2ge-5-8-${RUN_ID}" 2>/dev/null || true

echo "ft-1i2ge.5.8 e2e passed. Logs: ${LOG_FILE_REL}"
