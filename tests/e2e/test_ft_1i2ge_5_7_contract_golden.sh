#!/usr/bin/env bash
set -euo pipefail

# ft-1i2ge.5.7 — CLI/Robot/MCP contract tests with golden snapshots
# E2E scenario: validate mission contract golden tests compile, pass,
# are clippy-clean, and produce deterministic results across repeated runs.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_5_7_contract_golden"
CORRELATION_ID="ft-1i2ge.5.7-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.jsonl"
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
    --arg component "mission_contract_golden.e2e" \
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
  "mission contract golden e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi

# ── Test 1: Compile check ──────────────────────────────────────────
emit_log "running" "compile_check" "cargo_check" "none" \
  "none" "cargo check contract golden tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-7-${RUN_ID}" \
    cargo check -p frankenterm-core --features subprocess-bridge --test mission_contract_golden 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.compile.log" 2>&1
compile_rc=$?
set -e

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log "failed" "compile_check" "compilation_error" "COMPILE_FAIL" \
    "ft_1i2ge_5_7_${RUN_ID}.compile.log" "cargo check failed"
  echo "FAIL: compilation error" >&2
  exit 1
fi
emit_log "passed" "compile_check" "compilation_ok" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.compile.log" "compilation succeeded"

# ── Test 2: Contract golden tests pass ─────────────────────────────
emit_log "running" "golden_tests" "cargo_test" "none" \
  "none" "run mission contract golden tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-7-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test mission_contract_golden 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests.log" 2>&1
test_rc=$?
set -e

if [[ ${test_rc} -ne 0 ]]; then
  emit_log "failed" "golden_tests" "test_failure" "TEST_FAIL" \
    "ft_1i2ge_5_7_${RUN_ID}.tests.log" "contract golden tests failed"
  echo "FAIL: contract golden tests" >&2
  exit 1
fi

# Count passing contract tests.
contract_count=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests.log" || echo 0)

if [[ ${contract_count} -lt 30 ]]; then
  emit_log "failed" "golden_tests" "insufficient_test_coverage" "COVERAGE_LOW" \
    "ft_1i2ge_5_7_${RUN_ID}.tests.log" \
    "only ${contract_count} contract tests passed (expected >=30)"
  echo "FAIL: insufficient contract test coverage (${contract_count} < 30)" >&2
  exit 1
fi
emit_log "passed" "golden_tests" "all_tests_ok" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.tests.log" \
  "${contract_count} contract golden tests passed"

# ── Test 3: Clippy clean ──────────────────────────────────────────
emit_log "running" "clippy_check" "cargo_clippy" "none" \
  "none" "verify zero clippy warnings in contract golden tests"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-7-${RUN_ID}" \
    cargo clippy -p frankenterm-core --features subprocess-bridge \
    --test mission_contract_golden 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.clippy.log" 2>&1
clippy_rc=$?
set -e

contract_warnings=$(grep -c "mission_contract_golden.rs" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.clippy.log" || echo 0)
if [[ ${contract_warnings} -gt 0 ]]; then
  emit_log "failed" "clippy_check" "clippy_warnings" "CLIPPY_WARN" \
    "ft_1i2ge_5_7_${RUN_ID}.clippy.log" \
    "${contract_warnings} clippy warnings in mission_contract_golden.rs"
  echo "FAIL: clippy warnings in mission_contract_golden.rs" >&2
  exit 1
fi
emit_log "passed" "clippy_check" "clippy_clean" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.clippy.log" "zero clippy warnings"

# ── Test 4: Contract type coverage ─────────────────────────────────
emit_log "running" "type_coverage" "type_contract" "none" \
  "none" "validate contract tests cover key mission types"

# Check that key contract categories are tested.
missing_contracts=0

for pattern in \
  "contract_mission_trigger_variants" \
  "contract_operator_override_kind_serde" \
  "contract_operator_override_state" \
  "contract_assignment_set" \
  "contract_rejection_reason_variants" \
  "contract_mission_decision_golden_shape" \
  "contract_operator_status_report_golden_shape"; do
  if ! grep -q "${pattern}.*ok" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests.log"; then
    echo "MISSING: ${pattern}" >&2
    missing_contracts=$((missing_contracts + 1))
  fi
done

if [[ ${missing_contracts} -gt 0 ]]; then
  emit_log "failed" "type_coverage" "missing_contract_tests" "COVERAGE_MISSING" \
    "ft_1i2ge_5_7_${RUN_ID}.tests.log" \
    "${missing_contracts} key contract tests missing"
  echo "FAIL: ${missing_contracts} key contract tests missing" >&2
  exit 1
fi
emit_log "passed" "type_coverage" "all_contracts_covered" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.tests.log" "all key contract categories covered"

# ── Test 5: Determinism check (repeat run) ─────────────────────────
emit_log "running" "determinism" "repeat_run" "none" \
  "none" "verify contract test results are deterministic"

set +e
(
  cd "${ROOT_DIR}"
  CARGO_TARGET_DIR="target-e2e-1i2ge-5-7-${RUN_ID}" \
    cargo test -p frankenterm-core --features subprocess-bridge \
    --test mission_contract_golden 2>&1
) > "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests_repeat.log" 2>&1
repeat_rc=$?
set -e

if [[ ${repeat_rc} -ne 0 ]]; then
  emit_log "failed" "determinism" "repeat_run_failed" "REPEAT_FAIL" \
    "ft_1i2ge_5_7_${RUN_ID}.tests_repeat.log" "repeat run failed"
  echo "FAIL: repeat test run" >&2
  exit 1
fi

pass_count_1=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests.log" || echo 0)
pass_count_2=$(grep -c "ok$" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests_repeat.log" || echo 0)
if [[ ${pass_count_1} -ne ${pass_count_2} ]]; then
  emit_log "failed" "determinism" "count_mismatch" "DETERMINISM_FAIL" \
    "ft_1i2ge_5_7_${RUN_ID}.tests_repeat.log" \
    "pass count diverged: ${pass_count_1} vs ${pass_count_2}"
  echo "FAIL: non-deterministic test counts" >&2
  exit 1
fi
emit_log "passed" "determinism" "repeat_run_stable" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.tests_repeat.log" \
  "test counts stable: ${pass_count_1} == ${pass_count_2}"

# ── Test 6: Backward compatibility contracts ──────────────────────
emit_log "running" "backward_compat" "compat_contract" "none" \
  "none" "validate backward compatibility contract tests pass"

compat_count=$(grep -c "backward_compat.*ok" "${LOG_DIR}/ft_1i2ge_5_7_${RUN_ID}.tests.log" || echo 0)
if [[ ${compat_count} -lt 1 ]]; then
  emit_log "failed" "backward_compat" "missing_compat_test" "COMPAT_MISSING" \
    "ft_1i2ge_5_7_${RUN_ID}.tests.log" "backward compatibility tests not found"
  echo "FAIL: backward compatibility tests missing" >&2
  exit 1
fi
emit_log "passed" "backward_compat" "compat_verified" "none" \
  "ft_1i2ge_5_7_${RUN_ID}.tests.log" \
  "${compat_count} backward compatibility tests verified"

# ── Suite complete ─────────────────────────────────────────────────
emit_log "passed" "suite_complete" "all_scenarios_passed" "none" \
  "$(basename "${LOG_FILE}")" \
  "validated contract golden tests: compilation, ${contract_count} tests, clippy, type coverage, determinism, backward compat"

# Cleanup ephemeral target dir.
rm -rf "${ROOT_DIR}/target-e2e-1i2ge-5-7-${RUN_ID}" 2>/dev/null || true

echo "ft-1i2ge.5.7 e2e passed. Logs: ${LOG_FILE_REL}"
