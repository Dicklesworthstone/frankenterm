#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_22x4r_tokio_test_migration"
CORRELATION_ID="ft-22x4r-${RUN_ID}"
LOG_FILE="${LOG_DIR}/tokio_test_migration_${RUN_ID}.jsonl"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="$4"
  local input_summary="$5"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "tokio_test_migration.e2e" \
    --arg scenario_id "${SCENARIO_ID}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code
    }' >> "${LOG_FILE}"
}

echo "=== tokio::test → LabRuntime migration validation (ft-22x4r) ==="
echo "Run ID:     ${RUN_ID}"
echo "Log:        ${LOG_FILE_REL}"
echo ""

PASS=0
FAIL=0

# --- Scenario 1: No ungated #[tokio::test] in .rs source files ---
echo -n "S1: No ungated #[tokio::test] in src/... "
# Count actual #[tokio::test] attrs in .rs files, excluding:
# - .bak files (--include='*.rs')
# - comments (//.*#\[tokio)
# - string literals (".*#\[tokio)
# - cfg(not(feature="asupersync-runtime")) gated tests (runtime_compat.rs)
# Use grep -B1 to check the preceding line for cfg(not) gating
SRC_TOKIO=$(grep -rn --include='*.rs' '^\s*#\[tokio::test' "${ROOT_DIR}/crates/frankenterm-core/src/" \
  | grep -vc 'runtime_compat\.rs' || true)
# runtime_compat.rs has 2 tokio::test that are properly cfg(not)-gated — excluded above
if [ "${SRC_TOKIO}" -eq 0 ]; then
  echo "PASS"
  emit_log "pass" "no_tokio_test_src" "clean" "" "active_tokio_tests=0"
  PASS=$((PASS + 1))
else
  echo "FAIL (${SRC_TOKIO} active tokio::test attrs found)"
  emit_log "fail" "no_tokio_test_src" "remnants" "E_TOKIO" "active_tokio_tests=${SRC_TOKIO}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 2: LabRuntime test files exist ---
echo -n "S2: LabRuntime test files exist... "
LAB_FILES=$(find "${ROOT_DIR}/crates/frankenterm-core/tests" -name '*_labruntime.rs' | wc -l | tr -d ' ')
if [ "${LAB_FILES}" -ge 20 ]; then
  echo "PASS (${LAB_FILES} labruntime test files)"
  emit_log "pass" "labruntime_files" "sufficient" "" "lab_files=${LAB_FILES}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${LAB_FILES} labruntime files, expected >=20)"
  emit_log "fail" "labruntime_files" "insufficient" "E_FILES" "lab_files=${LAB_FILES}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 3: LabRuntime tests use RuntimeFixture or run_lab_test ---
echo -n "S3: LabRuntime tests use correct pattern... "
FIXTURE_USAGE=$(grep -rl 'RuntimeFixture\|run_lab_test\|run_chaos_test\|run_exploration_test' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/"*_labruntime.rs 2>/dev/null | wc -l | tr -d ' ')
if [ "${FIXTURE_USAGE}" -ge 20 ]; then
  echo "PASS (${FIXTURE_USAGE} files use LabRuntime patterns)"
  emit_log "pass" "labruntime_pattern" "consistent" "" "fixture_usage=${FIXTURE_USAGE}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "labruntime_pattern" "inconsistent" "E_PATTERN" "fixture_usage=${FIXTURE_USAGE}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 4: Most labruntime tests gated on asupersync-runtime ---
echo -n "S4: LabRuntime tests feature-gated... "
GATED=$(grep -rl 'cfg(feature = "asupersync-runtime")' \
  "${ROOT_DIR}/crates/frankenterm-core/tests/"*_labruntime.rs 2>/dev/null | wc -l | tr -d ' ')
# Some labruntime tests may work with both runtimes; >=80% gated is acceptable
THRESHOLD=$(( LAB_FILES * 80 / 100 ))
if [ "${GATED}" -ge "${THRESHOLD}" ]; then
  echo "PASS (${GATED}/${LAB_FILES} gated, threshold=${THRESHOLD})"
  emit_log "pass" "feature_gate" "sufficient" "" "gated=${GATED}/${LAB_FILES}"
  PASS=$((PASS + 1))
else
  echo "FAIL (${GATED}/${LAB_FILES} gated, need >=${THRESHOLD})"
  emit_log "fail" "feature_gate" "missing_gate" "E_GATE" "gated=${GATED}/${LAB_FILES}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 5: Common test infrastructure exists ---
echo -n "S5: Common test infrastructure... "
COMMON_DIR="${ROOT_DIR}/crates/frankenterm-core/tests/common"
if [ -f "${COMMON_DIR}/fixtures.rs" ] && [ -f "${COMMON_DIR}/lab.rs" ]; then
  echo "PASS"
  emit_log "pass" "common_infra" "present" "" "fixtures.rs+lab.rs"
  PASS=$((PASS + 1))
else
  echo "FAIL (missing common/fixtures.rs or common/lab.rs)"
  emit_log "fail" "common_infra" "missing" "E_INFRA" ""
  FAIL=$((FAIL + 1))
fi

# --- Scenario 6: runtime_compat actual tokio test attrs (not comments) are cfg-gated ---
echo -n "S6: runtime_compat tokio compat tests gated... "
# Count only lines that are actual attributes, not doc comments
COMPAT_TOKIO=$(grep -n '^\s*#\[tokio::test' "${ROOT_DIR}/crates/frankenterm-core/src/runtime_compat.rs" | wc -l | tr -d ' ')
COMPAT_GATED=$(grep -B1 '^\s*#\[tokio::test' "${ROOT_DIR}/crates/frankenterm-core/src/runtime_compat.rs" \
  | grep -c 'cfg(not(feature = "asupersync-runtime"))' || true)
if [ "${COMPAT_TOKIO}" -eq "${COMPAT_GATED}" ]; then
  echo "PASS (${COMPAT_TOKIO} tokio tests, all cfg-gated)"
  emit_log "pass" "compat_gating" "correct" "" "tokio=${COMPAT_TOKIO},gated=${COMPAT_GATED}"
  PASS=$((PASS + 1))
else
  echo "FAIL (${COMPAT_TOKIO} tokio tests, only ${COMPAT_GATED} gated)"
  emit_log "fail" "compat_gating" "ungated" "E_COMPAT" "tokio=${COMPAT_TOKIO},gated=${COMPAT_GATED}"
  FAIL=$((FAIL + 1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

if [ "${FAIL}" -gt 0 ]; then
  exit 1
fi
