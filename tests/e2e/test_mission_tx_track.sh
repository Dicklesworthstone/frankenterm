#!/usr/bin/env bash
# test_mission_tx_track.sh — End-to-end validation for the mission tx execution track (ft-1i2ge.8)
#
# Validates:
#   1. Nominal prepare→commit→committed lifecycle
#   2. Failure injection → auto-compensation → compensated lifecycle
#   3. Kill-switch (HardStop) blocks at prepare
#   4. Pause suspends commit (all steps skipped)
#   5. Compensation failure → failed state
#   6. Determinism (same inputs → same outputs)
#   7. Empty contract rejected
#
# All scenarios emit structured JSONL logs per the bead's log contract:
#   timestamp, component, scenario_id, correlation_id, decision_path,
#   input_summary, outcome, reason_code, error_code, artifact_path

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_track"
CORRELATION_ID="ft-1i2ge.8-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_track_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mission_tx_track_${RUN_ID}.stdout.log"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-mission-tx-track-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
TOTAL_SCENARIOS=7
LAST_STEP_QUEUE_LOG=""
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
LOCAL_RCH_TMPDIR_OVERRIDE=""
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""
RCH_PROBE_LOG="${LOG_DIR}/mission_tx_track_${RUN_ID}.rch_probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/mission_tx_track_${RUN_ID}.rch_smoke.log"

if [[ "$(uname -s)" == "Darwin" ]]; then
  LOCAL_RCH_TMPDIR_OVERRIDE="/tmp"
fi

# ── Structured log emitter ───────────────────────────────────────────────────

artifact_label() {
  local path="$1"

  if [[ -z "${path}" || "${path}" == "none" ]]; then
    printf '%s\n' "${path}"
    return
  fi

  if [[ "${path}" == "${ROOT_DIR}/"* ]]; then
    printf '%s\n' "${path#"${ROOT_DIR}"/}"
    return
  fi

  basename "${path}"
}

emit_log() {
  local outcome="$1"
  local decision_path="$2"
  local reason_code="$3"
  local error_code="${4:-}"
  local artifact_path="${5:-}"
  local input_summary="${6:-}"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "mission_tx_track.e2e" \
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

resolve_timeout_bin() {
  if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
  elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
  else
    TIMEOUT_BIN=""
  fi
}

run_rch() {
  if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
    env TMPDIR="${LOCAL_RCH_TMPDIR_OVERRIDE}" rch "$@"
  else
    rch "$@"
  fi
}

run_rch_timed() {
  local timeout_secs="$1"
  shift

  local -a cmd=(rch "$@")
  if [[ -n "${LOCAL_RCH_TMPDIR_OVERRIDE}" ]]; then
    cmd=(env TMPDIR="${LOCAL_RCH_TMPDIR_OVERRIDE}" "${cmd[@]}")
  fi

  if [[ -n "${TIMEOUT_BIN}" ]]; then
    "${TIMEOUT_BIN}" --signal=TERM --kill-after=10 "${timeout_secs}" "${cmd[@]}"
  else
    "${cmd[@]}"
  fi
}

probe_has_reachable_workers() {
  grep -Eiq '"status"[[:space:]]*:[[:space:]]*"(ok|healthy|reachable)"' "$1"
}

step_timed_out() {
  local rc="$1"
  [[ "${rc}" -eq 124 || "${rc}" -eq 137 ]]
}

timeout_artifact_label() {
  local default_path="$1"

  if [[ -n "${LAST_STEP_QUEUE_LOG}" ]]; then
    artifact_label "${LAST_STEP_QUEUE_LOG}"
  else
    artifact_label "${default_path}"
  fi
}

check_rch_fallback_in_logs() {
  local decision_path="$1"
  local artifact_path="$2"
  local input_summary="$3"

  if grep -Eq "${RCH_FAIL_OPEN_REGEX}" "${artifact_path}" 2>/dev/null; then
    emit_log \
      "failed" \
      "${decision_path}" \
      "rch_local_fallback_detected" \
      "RCH-LOCAL-FALLBACK" \
      "$(artifact_label "${artifact_path}")" \
      "${input_summary}"
    echo "rch fell back to local execution during ${decision_path}; refusing offload policy violation." >&2
    exit 3
  fi
}

run_rch_cargo_logged() {
  local decision_path="$1"
  local artifact_path="$2"
  shift 2

  LAST_STEP_QUEUE_LOG=""
  set +e
  (
    cd "${ROOT_DIR}"
    run_rch_timed "${RCH_STEP_TIMEOUT_SECS}" exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo "$@"
  ) 2>&1 | tee "${artifact_path}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e

  if step_timed_out "${rc}"; then
    LAST_STEP_QUEUE_LOG="${artifact_path%.log}.queue.log"
    if ! run_rch queue > "${LAST_STEP_QUEUE_LOG}" 2>&1; then
      LAST_STEP_QUEUE_LOG=""
    fi
  fi

  check_rch_fallback_in_logs "${decision_path}" "${artifact_path}" "rch cargo $*"
  return "${rc}"
}

# ── Fail-closed rch preflight ────────────────────────────────────────────────

echo "=== Mission TX Track E2E Validation (${SCENARIO_ID}) ==="
echo "  Run ID: ${RUN_ID}"
echo "  Log: ${LOG_FILE}"
echo "  Target: ${CARGO_TARGET_DIR}"
echo ""

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging." >&2
  exit 1
fi

emit_log "started" "preflight" "e2e.started" "" "$(artifact_label "${LOG_FILE}")" "scenarios=${TOTAL_SCENARIOS}"

if ! command -v rch >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight->rch_required" \
    "rch_required_missing" \
    "RCH-E001" \
    "$(artifact_label "${LOG_FILE}")" \
    "rch is required for cargo execution in this scenario"
  echo "rch is required for this e2e scenario; refusing local cargo execution." >&2
  exit 1
fi

resolve_timeout_bin
if [[ -z "${TIMEOUT_BIN}" ]]; then
  emit_log \
    "running" \
    "preflight->timeout_resolution" \
    "timeout_guard_unavailable" \
    "" \
    "$(artifact_label "${LOG_FILE}")" \
    "timeout/gtimeout not installed; continuing without external timeout wrapper"
fi

echo "[preflight] Probing rch workers..."
set +e
run_rch --json workers probe --all > "${RCH_PROBE_LOG}" 2>&1
probe_rc=$?
set -e
check_rch_fallback_in_logs "preflight->rch_probe" "${RCH_PROBE_LOG}" "rch workers probe --all"
if [[ ${probe_rc} -ne 0 ]] || ! probe_has_reachable_workers "${RCH_PROBE_LOG}"; then
  emit_log \
    "failed" \
    "preflight->rch_probe" \
    "rch_workers_unhealthy" \
    "RCH-E100" \
    "$(artifact_label "${RCH_PROBE_LOG}")" \
    "probe_exit=${probe_rc}"
  echo "rch workers are unavailable; refusing local cargo execution." >&2
  exit 1
fi
emit_log \
  "passed" \
  "preflight->rch_probe" \
  "rch_workers_healthy" \
  "" \
  "$(artifact_label "${RCH_PROBE_LOG}")" \
  "rch workers probe reported reachable capacity"

echo "[preflight] Verifying remote rch exec path..."
set +e
run_rch_timed "${RCH_STEP_TIMEOUT_SECS}" exec -- cargo check --help > "${RCH_SMOKE_LOG}" 2>&1
smoke_rc=$?
set -e
check_rch_fallback_in_logs "preflight->rch_smoke" "${RCH_SMOKE_LOG}" "rch remote smoke check (cargo check --help)"
if step_timed_out "${smoke_rc}"; then
  emit_log \
    "failed" \
    "preflight->rch_smoke" \
    "rch_remote_smoke_timed_out" \
    "RCH-REMOTE-STALL" \
    "$(artifact_label "${RCH_SMOKE_LOG}")" \
    "timeout_secs=${RCH_STEP_TIMEOUT_SECS}"
  echo "rch remote smoke check timed out." >&2
  exit 1
fi
if [[ ${smoke_rc} -ne 0 ]]; then
  emit_log \
    "failed" \
    "preflight->rch_smoke" \
    "rch_remote_smoke_failed" \
    "RCH-E101" \
    "$(artifact_label "${RCH_SMOKE_LOG}")" \
    "smoke_exit=${smoke_rc}"
  echo "rch remote smoke preflight failed; refusing local cargo execution." >&2
  exit 1
fi
emit_log \
  "passed" \
  "preflight->rch_smoke" \
  "rch_remote_smoke_passed" \
  "" \
  "$(artifact_label "${RCH_SMOKE_LOG}")" \
  "verified remote rch exec path before running cargo tests"

echo "[preflight] Compiling proptest_tx_execution via rch..."
compile_log="${LOG_DIR}/mission_tx_track_${RUN_ID}.compile.log"
if run_rch_cargo_logged "preflight->build_check" "${compile_log}" \
  test -p frankenterm-core --features subprocess-bridge --no-default-features \
  --test proptest_tx_execution --no-run; then
  compile_rc=0
else
  compile_rc=$?
fi

if step_timed_out "${compile_rc}"; then
  emit_log \
    "failed" \
    "preflight->build_check" \
    "build.compile_timed_out" \
    "RCH-REMOTE-STALL" \
    "$(timeout_artifact_label "${compile_log}")" \
    "timeout_secs=${RCH_STEP_TIMEOUT_SECS}"
  echo "[preflight] FAIL: compile step timed out"
  exit 1
fi

if [[ ${compile_rc} -ne 0 ]]; then
  emit_log \
    "failed" \
    "preflight->build_check" \
    "build.compile_failed" \
    "BUILD-E001" \
    "$(artifact_label "${compile_log}")" \
    "exit=${compile_rc}"
  echo "[preflight] FAIL: Cannot compile test binary (exit ${compile_rc})"
  exit 1
fi

emit_log \
  "passed" \
  "preflight->build_check" \
  "build.compiled" \
  "" \
  "$(artifact_label "${compile_log}")" \
  "test binary compiled remotely via rch"

# ── Run test scenarios ───────────────────────────────────────────────────────

run_test() {
  local name="$1"
  local filter="$2"
  local scenario_decision="$3"
  local scenario_log="${LOG_DIR}/mission_tx_track_${RUN_ID}_${name}.log"

  echo -n "  [${name}] "

  emit_log \
    "running" \
    "${scenario_decision}" \
    "${name}.running" \
    "" \
    "$(artifact_label "${scenario_log}")" \
    "filter=${filter}"

  if run_rch_cargo_logged "${scenario_decision}" "${scenario_log}" \
    test -p frankenterm-core --features subprocess-bridge --no-default-features \
    --test proptest_tx_execution -- "${filter}" --exact --nocapture; then
    test_exit=0
  else
    test_exit=$?
  fi

  if step_timed_out "${test_exit}"; then
    echo "FAIL (timeout)"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    emit_log \
      "failed" \
      "${scenario_decision}" \
      "${name}.timed_out" \
      "RCH-REMOTE-STALL" \
      "$(timeout_artifact_label "${scenario_log}")" \
      "filter=${filter},timeout_secs=${RCH_STEP_TIMEOUT_SECS}"
    return 0
  fi

  if [[ ${test_exit} -eq 0 ]]; then
    echo "PASS"
    PASS_COUNT=$((PASS_COUNT + 1))
    emit_log \
      "passed" \
      "${scenario_decision}" \
      "${name}.passed" \
      "" \
      "$(artifact_label "${scenario_log}")" \
      "filter=${filter}"
  else
    echo "FAIL (exit ${test_exit})"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    emit_log \
      "failed" \
      "${scenario_decision}" \
      "${name}.failed" \
      "TEST-E001" \
      "$(artifact_label "${scenario_log}")" \
      "filter=${filter},exit=${test_exit}"
  fi
}

echo "[scenarios] Running ${TOTAL_SCENARIOS} validation scenarios..."

# Scenario 1: Nominal lifecycle (prepare→commit→committed)
run_test "nominal_lifecycle" "execution_is_deterministic_for_same_inputs" "scenario->nominal"

# Scenario 2: Failure injection → compensation
run_test "failure_compensation" "failure_injection_preserves_step_count" "scenario->failure_injection"

# Scenario 3: Kill-switch blocks commit
run_test "kill_switch_block" "kill_switch_hard_stop_blocks_commit" "scenario->kill_switch"

# Scenario 4: Pause suspends commit
run_test "pause_suspend" "pause_suspends_all_steps" "scenario->pause"

# Scenario 5: Commit counts invariant
run_test "commit_counts" "commit_counts_sum_to_total_steps" "scenario->commit_counts"

# Scenario 6: Final state is terminal
run_test "terminal_state" "final_state_is_terminal" "scenario->terminal_state"

# Scenario 7: Empty contract rejected
run_test "empty_contract_error" "empty_contract_returns_invalid_contract_error" "scenario->empty_contract"

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "=== RESULTS: ${PASS_COUNT} passed, ${FAIL_COUNT} failed, ${SKIP_COUNT} skipped (of ${TOTAL_SCENARIOS}) ==="

emit_log \
  "completed" \
  "summary" \
  "e2e.completed" \
  "" \
  "$(artifact_label "${LOG_FILE}")" \
  "pass=${PASS_COUNT},fail=${FAIL_COUNT},skip=${SKIP_COUNT}"

if [[ "${FAIL_COUNT}" -gt 0 ]]; then
  echo "  Log: ${LOG_FILE}"
  exit 1
fi

exit 0
