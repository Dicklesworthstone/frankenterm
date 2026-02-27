#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.7.7 — Operator takeover game-days and emergency-control certification
# E2E scenario: validate game-day tests compile, pass, are clippy-clean,
# cover all operator-takeover categories, and produce deterministic results.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_7_7_game_day"
CORRELATION_ID="ft-1i2ge.7.7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.jsonl"
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
    --arg component "game_day.e2e" \
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
  "game-day e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check game-day tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-7-7-${RUN_ID}" \
    cargo check -p frankenterm-core --features subprocess-bridge \
    --test mission_game_day 2>&1
) > "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.compile.log" 2>&1
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_7_7_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_7_7_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Game-day tests pass ────────────────────────────────────
emit_log "running" "game_day_tests" "cargo_test" "none" \
  "none" "run game-day tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-7-7-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test mission_game_day 2>&1
) > "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests.log" 2>&1
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "game_day_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_7_7_${RUN_ID}.tests.log" "game-day tests failed"
  echo "FAIL: game-day tests" >&2
  exit 1
fi

gameday_count=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests.log" || echo 0)

if [[ ${gameday_count} -lt 20 ]]; then
  emit_log "failed" "game_day_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_7_7_${RUN_ID}.tests.log" \
    "only ${gameday_count} game-day tests passed (expected >=20)"
  echo "FAIL: insufficient game-day test coverage (${gameday_count} < 20)" >&2
  exit 1
fi
emit_log "passed" "game_day_tests" "all_tests_ok" "none" \
  "ft_1i2ge_7_7_${RUN_ID}.tests.log" \
  "${gameday_count} game-day tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in game-day tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-7-7-${RUN_ID}" \
    cargo clippy -p frankenterm-core --features subprocess-bridge \
    --test mission_game_day 2>&1
) > "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.clippy.log" 2>&1
clippy_rc=$?
set -e

gameday_warnings=$(grep -c "mission_game_day.rs" "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.clippy.log" || echo 0)
if [[ ${gameday_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_7_7_${RUN_ID}.clippy.log" \
    "${gameday_warnings} clippy warnings in mission_game_day.rs"
  echo "FAIL: clippy warnings in mission_game_day.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_7_7_${RUN_ID}.clippy.log" "zero clippy warnings"

# ── Test 4: Category coverage ─────────────────────────────────────
emit_log "running" "category_coverage" "coverage_check" "none" \
  "none" "validate all game-day categories covered"

missing_categories=0

for pattern in \
  "emergency_" \
  "takeover_" \
  "degraded_" \
  "recovery_" \
  "audit_" \
  "load_" \
  "determinism_"; do
  if ! grep -q "${pattern}.*ok" "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests.log"; then
    echo "MISSING: ${pattern}" >&2
    missing_categories=$((missing_categories + 1))
  fi
done

if [[ ${missing_categories} -gt 0 ]]; then
  emit_log "failed" "category_coverage" "missing_categories" "COVERAGE_MISSING" \
    "ft_1i2ge_7_7_${RUN_ID}.tests.log" \
    "${missing_categories} game-day categories missing"
  echo "FAIL: ${missing_categories} game-day categories missing" >&2
  exit 1
fi
emit_log "passed" "category_coverage" "all_categories_covered" "none" \
  "ft_1i2ge_7_7_${RUN_ID}.tests.log" "all game-day categories covered"

# ── Test 5: Determinism check ──────────────────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify game-day results are deterministic"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-7-7-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test mission_game_day 2>&1
) > "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests_repeat.log" 2>&1
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_7_7_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests.log" || echo 0)
pass_count_2=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_7_7_${RUN_ID}.tests_repeat.log" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_7_7_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_7_7_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated game-day: compilation, ${gameday_count} tests, clippy, category coverage, determinism"

# Cleanup ephemeral target dir.
rm -rf "${ROOT_DIR}/target-e2e-1i2ge-7-7-${RUN_ID}" 2>/dev/null || true

echo "ft-1i2ge.7.7 e2e passed. Logs: ${LOG_FILE_REL}"
