#!/usr/bin/env bash
# E2E smoke test: Intent Ledger and Causal Receipt Persistence (ft-1i2ge.8.2)
#
# Validates ledger hash chain integrity, invalid-path detection, serde
# roundtrips, query surfaces, and lifecycle flows using the current
# tx_idempotency Rust tests as ground truth.
#
# Summary JSON: {"test":"mission_tx_ledger","scenario":N,"status":"pass|fail"}

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_ledger"
CORRELATION_ID="ft-1i2ge.8.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_ledger_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/mission_tx_ledger_${RUN_ID}.stdout.log"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-mission-tx-ledger-${RUN_ID}"
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
TOTAL_SCENARIOS=6
LAST_STEP_QUEUE_LOG=""
RCH_FAIL_OPEN_REGEX='\[RCH\][[:space:]]+local|Remote execution failed: .*running locally|running locally|Failed to connect to ubuntu@|too long for Unix domain socket'
LOCAL_RCH_TMPDIR_OVERRIDE=""
RCH_STEP_TIMEOUT_SECS="${RCH_STEP_TIMEOUT_SECS:-900}"
TIMEOUT_BIN=""
RCH_PROBE_LOG="${LOG_DIR}/mission_tx_ledger_${RUN_ID}.rch_probe.log"
RCH_SMOKE_LOG="${LOG_DIR}/mission_tx_ledger_${RUN_ID}.rch_smoke.log"

if [[ "$(uname -s)" == "Darwin" ]]; then
  LOCAL_RCH_TMPDIR_OVERRIDE="/tmp"
fi

pass() { PASS_COUNT=$((PASS_COUNT + 1)); echo "  PASS: $1"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); echo "  FAIL: $1"; }

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
    --arg component "mission_tx_ledger.e2e" \
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

slugify() {
  local value="$1"
  value="${value//::/_}"
  value="${value// /_}"
  value="${value//\//_}"
  value="${value//[^[:alnum:]_.-]/_}"
  printf '%s\n' "${value}"
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

run_exact_group() {
  local scenario_num="$1"
  local title="$2"
  local check_name="$3"
  local pass_message="$4"
  local decision_path="$5"
  shift 5

  local last_artifact="none"

  echo ""
  echo "--- Scenario ${scenario_num}: ${title} ---"

  for filter in "$@"; do
    local filter_slug
    local filter_log
    filter_slug="$(slugify "${filter}")"
    filter_log="${LOG_DIR}/mission_tx_ledger_${RUN_ID}_${scenario_num}_${filter_slug}.log"
    last_artifact="${filter_log}"

    emit_log \
      "running" \
      "${decision_path}" \
      "${check_name}.running" \
      "" \
      "$(artifact_label "${filter_log}")" \
      "filter=${filter}"

    if run_rch_cargo_logged "${decision_path}" "${filter_log}" \
      test -p frankenterm-core --lib "${filter}" -- --exact --nocapture; then
      continue
    fi

    local rc=$?
    if step_timed_out "${rc}"; then
      fail "${pass_message}"
      emit_log \
        "failed" \
        "${decision_path}" \
        "${check_name}.timed_out" \
        "RCH-REMOTE-STALL" \
        "$(timeout_artifact_label "${filter_log}")" \
        "filter=${filter},timeout_secs=${RCH_STEP_TIMEOUT_SECS}"
    else
      fail "${pass_message}"
      emit_log \
        "failed" \
        "${decision_path}" \
        "${check_name}.failed" \
        "TEST-E001" \
        "$(artifact_label "${filter_log}")" \
        "filter=${filter},exit=${rc}"
    fi
    return 0
  done

  pass "${pass_message}"
  emit_log \
    "passed" \
    "${decision_path}" \
    "${check_name}.passed" \
    "" \
    "$(artifact_label "${last_artifact}")" \
    "group_size=$#"
  printf '{"test":"mission_tx_ledger","scenario":%s,"check":"%s","status":"pass"}\n' "${scenario_num}" "${check_name}"
}

run_broad_scenario() {
  local scenario_num="$1"
  local title="$2"
  local check_name="$3"
  local pass_message="$4"
  local decision_path="$5"
  shift 5

  local scenario_log
  scenario_log="${LOG_DIR}/mission_tx_ledger_${RUN_ID}_${scenario_num}_$(slugify "${check_name}").log"

  echo ""
  echo "--- Scenario ${scenario_num}: ${title} ---"

  emit_log \
    "running" \
    "${decision_path}" \
    "${check_name}.running" \
    "" \
    "$(artifact_label "${scenario_log}")" \
    "cargo_args=$*"

  if run_rch_cargo_logged "${decision_path}" "${scenario_log}" "$@"; then
    pass "${pass_message}"
    emit_log \
      "passed" \
      "${decision_path}" \
      "${check_name}.passed" \
      "" \
      "$(artifact_label "${scenario_log}")" \
      "cargo_args=$*"
    printf '{"test":"mission_tx_ledger","scenario":%s,"check":"%s","status":"pass"}\n' "${scenario_num}" "${check_name}"
    return 0
  fi

  local rc=$?
  if step_timed_out "${rc}"; then
    fail "${pass_message}"
    emit_log \
      "failed" \
      "${decision_path}" \
      "${check_name}.timed_out" \
      "RCH-REMOTE-STALL" \
      "$(timeout_artifact_label "${scenario_log}")" \
      "timeout_secs=${RCH_STEP_TIMEOUT_SECS}"
  else
    fail "${pass_message}"
    emit_log \
      "failed" \
      "${decision_path}" \
      "${check_name}.failed" \
      "TEST-E001" \
      "$(artifact_label "${scenario_log}")" \
      "cargo_args=$*,exit=${rc}"
  fi
}

# ── Fail-closed rch preflight ────────────────────────────────────────────────

echo "=== Intent Ledger E2E Suite ==="
echo "  Run ID: ${RUN_ID}"
echo "  Log: ${LOG_FILE}"
echo "  Target: ${CARGO_TARGET_DIR}"

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

echo "[preflight] Compiling tx_idempotency tests via rch..."
compile_log="${LOG_DIR}/mission_tx_ledger_${RUN_ID}.compile.log"
if run_rch_cargo_logged "preflight->build_check" "${compile_log}" \
  test -p frankenterm-core --lib tx_idempotency --no-run; then
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
  echo "[preflight] FAIL: Cannot compile ledger test binary (exit ${compile_rc})"
  exit 1
fi

emit_log \
  "passed" \
  "preflight->build_check" \
  "build.compiled" \
  "" \
  "$(artifact_label "${compile_log}")" \
  "tx_idempotency tests compiled remotely via rch"

# ── Scenario 1: Ledger creation and hash chain ──────────────────────────
run_exact_group \
  1 \
  "Hash Chain Integrity" \
  "hash_chain" \
  "Hash chain integrity, append/lookup, and initialization" \
  "scenario->hash_chain" \
  "tx_idempotency::tests::ledger_new_empty" \
  "tx_idempotency::tests::ledger_append_and_lookup" \
  "tx_idempotency::tests::ledger_hash_chain_integrity"

# ── Scenario 2: Invalid-path detection ───────────────────────────────────
run_exact_group \
  2 \
  "Invalid Path Detection" \
  "invalid_paths" \
  "Duplicate, sealed-ledger, and invalid-transition failures" \
  "scenario->invalid_paths" \
  "tx_idempotency::tests::ledger_duplicate_rejected" \
  "tx_idempotency::tests::ledger_sealed_rejects_append" \
  "tx_idempotency::tests::ledger_invalid_phase_transition"

# ── Scenario 3: Serde roundtrips ─────────────────────────────────────────
run_exact_group \
  3 \
  "Serde Roundtrips" \
  "serde" \
  "Key, outcome, phase, ledger, resume, policy, error, and record serde roundtrips" \
  "scenario->serde" \
  "tx_idempotency::tests::key_serde_roundtrip" \
  "tx_idempotency::tests::outcome_serde_roundtrip" \
  "tx_idempotency::tests::phase_serde_roundtrip" \
  "tx_idempotency::tests::ledger_serde_roundtrip" \
  "tx_idempotency::tests::resume_serde_roundtrip" \
  "tx_idempotency::tests::policy_serde_roundtrip" \
  "tx_idempotency::tests::error_serde_roundtrip" \
  "tx_idempotency::tests::record_serde_roundtrip"

# ── Scenario 4: Query surfaces ───────────────────────────────────────────
run_exact_group \
  4 \
  "Query Surfaces" \
  "queries" \
  "Completed/failed step queries, pending step views, and index rebuild" \
  "scenario->queries" \
  "tx_idempotency::tests::ledger_completed_and_failed_steps" \
  "tx_idempotency::tests::ledger_pending_step_ids" \
  "tx_idempotency::tests::ledger_rebuild_index"

# ── Scenario 5: Lifecycle flows ───────────────────────────────────────────
run_exact_group \
  5 \
  "Lifecycle Flows" \
  "lifecycle" \
  "Full lifecycle success path and partial-failure resume path" \
  "scenario->lifecycle" \
  "tx_idempotency::tests::full_tx_lifecycle" \
  "tx_idempotency::tests::partial_failure_and_resume"

# ── Scenario 6: Full unit test suite ─────────────────────────────────────
run_broad_scenario \
  6 \
  "Full Unit Test Suite" \
  "full_suite" \
  "All tx idempotency ledger unit tests" \
  "scenario->full_suite" \
  test -p frankenterm-core --lib tx_idempotency -- --nocapture

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
TOTAL=$((PASS_COUNT + FAIL_COUNT))
STATUS="pass"
if [[ "${FAIL_COUNT}" -gt 0 ]]; then
    STATUS="fail"
fi

echo "=== Results: ${PASS_COUNT}/${TOTAL} passed ==="
emit_log \
  "completed" \
  "summary" \
  "e2e.completed" \
  "" \
  "$(artifact_label "${LOG_FILE}")" \
  "pass=${PASS_COUNT},fail=${FAIL_COUNT},skip=${SKIP_COUNT}"
echo "{\"test\":\"mission_tx_ledger\",\"contract_pass\":$([[ "${FAIL_COUNT}" -eq 0 ]] && echo true || echo false),\"scenario_pass\":${PASS_COUNT},\"status\":\"${STATUS}\"}"

exit "$FAIL_COUNT"
