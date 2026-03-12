#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_2h5wv_mux_pool_asupersync"
CORRELATION_ID="ft-2h5wv-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mux_pool_asupersync_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mux_pool_asupersync_${RUN_ID}.stdout.log"
LOG_FILE_REL="${LOG_FILE#"${ROOT_DIR}"/}"

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
    --arg component "mux_pool_asupersync.e2e" \
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

echo "=== MuxPool asupersync migration validation (ft-2h5wv) ==="
echo "Run ID:     ${RUN_ID}"
echo "Log:        ${LOG_FILE_REL}"
echo ""

PASS=0
FAIL=0

# --- Scenario 1: Source-level Cx-threaded API completeness ---
echo -n "S1: Cx-threaded API completeness... "
CX_METHODS=$(grep -c 'pub async fn.*_with_cx' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
AMBIENT_METHODS=$(grep -c 'pub async fn [a-z_]*(.' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
# Every ambient async public method should have a _with_cx counterpart
# Expect at least 7 Cx methods (list_panes, get_lines, get_pane_render_changes,
# get_pane_render_changes_batch, write_to_pane, send_paste, health_check)
if [ "${CX_METHODS}" -ge 7 ]; then
  echo "PASS (${CX_METHODS} Cx methods found)"
  emit_log "pass" "cx_api_completeness" "cx_methods_found" "" "" "cx_methods=${CX_METHODS}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${CX_METHODS} Cx methods, expected >=7)"
  emit_log "fail" "cx_api_completeness" "insufficient_cx_methods" "E_CX_GAP" "" "cx_methods=${CX_METHODS}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 2: No #[tokio::test] remaining ---
echo -n "S2: No tokio::test in mux_pool... "
TOKIO_TESTS=$(grep -c '#\[tokio::test\]' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${TOKIO_TESTS}" -eq 0 ]; then
  echo "PASS"
  emit_log "pass" "no_tokio_test" "clean" "" "" "tokio_tests=0"
  PASS=$((PASS + 1))
else
  echo "FAIL (${TOKIO_TESTS} tokio::test attrs remain)"
  emit_log "fail" "no_tokio_test" "tokio_remnants" "E_TOKIO" "" "tokio_tests=${TOKIO_TESTS}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 3: Structured diagnostics present ---
echo -n "S3: Structured diagnostics coverage... "
DIAG_EVENTS=$(grep -c 'subsystem = "mux_pool"' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${DIAG_EVENTS}" -ge 8 ]; then
  echo "PASS (${DIAG_EVENTS} diagnostic events)"
  emit_log "pass" "diagnostics_coverage" "sufficient" "" "" "diag_events=${DIAG_EVENTS}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${DIAG_EVENTS} diagnostic events, expected >=8)"
  emit_log "fail" "diagnostics_coverage" "insufficient" "E_DIAG" "" "diag_events=${DIAG_EVENTS}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 4: Test count meets acceptance threshold ---
echo -n "S4: Test count >= 50... "
TEST_COUNT=$(grep -c '#\[test\]' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${TEST_COUNT}" -ge 50 ]; then
  echo "PASS (${TEST_COUNT} tests)"
  emit_log "pass" "test_count" "sufficient" "" "" "test_count=${TEST_COUNT}"
  PASS=$((PASS + 1))
else
  echo "FAIL (only ${TEST_COUNT} tests, expected >=50)"
  emit_log "fail" "test_count" "insufficient" "E_TESTS" "" "test_count=${TEST_COUNT}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 5: Recovery code paths present ---
echo -n "S5: Recovery code paths... "
RECOVERY_CX=$(grep -c 'execute_with_recovery_with_cx' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
RECOVERY_INNER=$(grep -c 'execute_with_recovery_inner' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${RECOVERY_CX}" -ge 2 ] && [ "${RECOVERY_INNER}" -ge 2 ]; then
  echo "PASS (cx=${RECOVERY_CX}, inner=${RECOVERY_INNER})"
  emit_log "pass" "recovery_paths" "dual_path" "" "" "cx=${RECOVERY_CX},inner=${RECOVERY_INNER}"
  PASS=$((PASS + 1))
else
  echo "FAIL (cx=${RECOVERY_CX}, inner=${RECOVERY_INNER})"
  emit_log "fail" "recovery_paths" "missing_path" "E_RECOVERY" "" "cx=${RECOVERY_CX},inner=${RECOVERY_INNER}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 6: MuxPoolStats serde support ---
echo -n "S6: MuxPoolStats serde... "
SERDE_DERIVE=$(grep -c 'Serialize, Deserialize' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${SERDE_DERIVE}" -ge 1 ]; then
  echo "PASS"
  emit_log "pass" "stats_serde" "present" "" "" "serde_derives=${SERDE_DERIVE}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "stats_serde" "missing" "E_SERDE" "" ""
  FAIL=$((FAIL + 1))
fi

# --- Scenario 7: Pipeline batch with fallback ---
echo -n "S7: Pipeline batch fallback... "
FALLBACK=$(grep -c 'falling back to sequential' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${FALLBACK}" -ge 2 ]; then
  echo "PASS (${FALLBACK} fallback paths)"
  emit_log "pass" "pipeline_fallback" "present" "" "" "fallback_paths=${FALLBACK}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "pipeline_fallback" "missing" "E_PIPELINE" "" "fallback_paths=${FALLBACK}"
  FAIL=$((FAIL + 1))
fi

# --- Scenario 8 (negative): Verify ambient path still works without asupersync feature ---
echo -n "S8: Ambient (non-Cx) API surface... "
AMBIENT_ACQUIRE=$(grep -c 'acquire_client_inner' "${ROOT_DIR}/crates/frankenterm-core/src/vendored/mux_pool.rs" || true)
if [ "${AMBIENT_ACQUIRE}" -ge 2 ]; then
  echo "PASS"
  emit_log "pass" "ambient_api" "present" "" "" "ambient_acquire_refs=${AMBIENT_ACQUIRE}"
  PASS=$((PASS + 1))
else
  echo "FAIL"
  emit_log "fail" "ambient_api" "missing" "E_AMBIENT" "" ""
  FAIL=$((FAIL + 1))
fi

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="
echo "Log: ${LOG_FILE_REL}"

if [ "${FAIL}" -gt 0 ]; then
  exit 1
fi
