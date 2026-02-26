#!/usr/bin/env bash
set -euo pipefail

# E2E test for mission MCP tools surface [ft-1i2ge.5.3]
# Tests: ft mission status/explain/pause/resume/abort with JSON output
# Covers: nominal paths, failure injection (invalid state transitions), recovery

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_mcp_tools"
CORRELATION_ID="ft-1i2ge.5.3-${RUN_ID}"
TARGET_DIR="target-rch-mission-mcp-tools-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_mcp_tools_${RUN_ID}.jsonl"
STDOUT_BASENAME="mission_mcp_tools_${RUN_ID}"
WORKSPACE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ft-mission-mcp-tools.XXXXXX")"
MISSION_PATH="${WORKSPACE_DIR}/.ft/mission/active.json"

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
    --arg component "mission_mcp_tools.e2e" \
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

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured e2e validation" >&2
  exit 1
fi

if ! command -v rch >/dev/null 2>&1; then
  echo "rch is required; refusing local cargo execution" >&2
  exit 1
fi

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "validate ft mission lifecycle commands (status, explain, pause, resume, abort)"

if ! rch workers probe --all --json \
  | jq -e '[.data[] | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length > 0' \
    >/dev/null 2>&1; then
  emit_log \
    "failed" \
    "preflight_rch_workers" \
    "rch_workers_unreachable" \
    "remote_worker_unavailable" \
    "$(basename "${LOG_FILE}")" \
    "no healthy rch workers available"
  exit 1
fi

# Create mission fixture: Running state with 2 assignments (1 succeeded, 1 pending)
mkdir -p "$(dirname "${MISSION_PATH}")"
cat >"${MISSION_PATH}" <<'JSON'
{
  "mission_version": 1,
  "mission_id": "m-e2e-mcp-tools",
  "title": "E2E MCP Tools Validation Mission",
  "workspace_id": "ws-e2e-test",
  "ownership": {
    "planner": "e2e-planner",
    "dispatcher": "e2e-dispatcher",
    "operator": "e2e-operator"
  },
  "lifecycle_state": "Running",
  "created_at_ms": 1704200000000,
  "candidates": [
    {
      "action_id": "c-1",
      "action": { "SendText": { "pane_id": 7, "text": "echo hello", "paste_mode": false } },
      "score": 100,
      "priority": 1,
      "source": "planner"
    },
    {
      "action_id": "c-2",
      "action": { "SendText": { "pane_id": 8, "text": "echo world", "paste_mode": true } },
      "score": 80,
      "priority": 2,
      "source": "planner"
    }
  ],
  "assignments": [
    {
      "assignment_id": "a-1",
      "candidate_id": "c-1",
      "assignee": "agent-alpha",
      "assigned_by": "Planner",
      "approval_state": "NotRequired",
      "outcome": {
        "Success": {
          "reason_code": "ok",
          "completed_at_ms": 1704200010000
        }
      },
      "created_at_ms": 1704200001000
    },
    {
      "assignment_id": "a-2",
      "candidate_id": "c-2",
      "assignee": "agent-beta",
      "assigned_by": "Planner",
      "approval_state": "NotRequired",
      "created_at_ms": 1704200002000
    }
  ]
}
JSON

run_mission_json() {
  local label="$1"
  shift
  local stdout_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stdout.json"
  local stderr_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stderr.log"

  set +e
  (
    cd "${ROOT_DIR}"
    rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" \
      cargo run -q -p frankenterm --bin ft -- \
      --workspace "${WORKSPACE_DIR}" \
      mission \
      -f json \
      "$@"
  ) >"${stdout_file}" 2>"${stderr_file}"
  local rc=$?
  set -e

  LAST_STDOUT_FILE="${stdout_file}"
  LAST_STDERR_FILE="${stderr_file}"
  LAST_RC="${rc}"
  return "${rc}"
}

assert_jq_true() {
  local decision_path="$1"
  local jq_expr="$2"
  local input_summary="$3"
  if jq -e "${jq_expr}" "${LAST_STDOUT_FILE}" >/dev/null 2>&1; then
    emit_log "passed" "${decision_path}" "assertion_satisfied" "none" \
      "$(basename "${LAST_STDOUT_FILE}")" "${input_summary}"
    return
  fi

  emit_log "failed" "${decision_path}" "assertion_failed" "json_contract_mismatch" \
    "$(basename "${LAST_STDOUT_FILE}")" "${input_summary}"
  echo "Assertion failed (${decision_path}): ${jq_expr}" >&2
  echo "Stdout: ${LAST_STDOUT_FILE}" >&2
  echo "Stderr: ${LAST_STDERR_FILE}" >&2
  exit 1
}

LAST_STDOUT_FILE=""
LAST_STDERR_FILE=""
LAST_RC=0

# ── Scenario 1: Mission status (nominal, Running state) ────────────────
emit_log "running" "mission_status_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission status --mission-file <path>"

run_mission_json "status_nominal" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "status_nominal" \
  '.ok == true and .data.mission_id == "m-e2e-mcp-tools" and .data.lifecycle_state == "Running" and .data.assignment_count == 2' \
  "validate mission status nominal: correct id, state, and counts"

# Capture status signature for determinism check
STATUS_SIG_1="$(jq -c '{mission_id:.data.mission_id,lifecycle_state:.data.lifecycle_state,assignment_count:.data.assignment_count,candidate_count:.data.candidate_count}' "${LAST_STDOUT_FILE}")"

# ── Scenario 2: Determinism — repeat status yields identical envelope ──
run_mission_json "status_repeat" status --mission-file "${MISSION_PATH}"
STATUS_SIG_2="$(jq -c '{mission_id:.data.mission_id,lifecycle_state:.data.lifecycle_state,assignment_count:.data.assignment_count,candidate_count:.data.candidate_count}' "${LAST_STDOUT_FILE}")"
if [[ "${STATUS_SIG_1}" != "${STATUS_SIG_2}" ]]; then
  emit_log "failed" "determinism_check" "signature_mismatch" "repeat_run_instability" \
    "$(basename "${LAST_STDOUT_FILE}")" "mission status signatures diverged across repeat run"
  echo "Determinism check failed: status signatures differ" >&2
  exit 1
fi
emit_log "passed" "determinism_check" "repeat_run_stable" "none" \
  "$(basename "${LAST_STDOUT_FILE}")" "mission status signatures stable across repeat run"

# ── Scenario 3: Mission explain (Running state) ────────────────────────
emit_log "running" "mission_explain_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission explain --mission-file <path>"

run_mission_json "explain_nominal" explain --mission-file "${MISSION_PATH}"
assert_jq_true \
  "explain_nominal" \
  '.ok == true and .data.mission_id == "m-e2e-mcp-tools" and .data.lifecycle_state == "Running"' \
  "validate mission explain nominal output"

# ── Scenario 4: Pause mission (Running → Paused) ──────────────────────
emit_log "running" "mission_pause_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission pause --reason overload"

run_mission_json "pause_nominal" pause --mission-file "${MISSION_PATH}" --reason "operator_overload"
assert_jq_true \
  "pause_nominal" \
  '.ok == true and .data.command == "pause" and .data.lifecycle_state == "Paused"' \
  "validate pause transition Running → Paused"

# Verify file was persisted with new state
run_mission_json "status_after_pause" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "status_after_pause" \
  '.ok == true and .data.lifecycle_state == "Paused"' \
  "validate mission file persisted Paused state"

# ── Scenario 5: Failure injection — pause when already Paused ─────────
emit_log "running" "pause_failure_injection" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission pause (already paused — expect error)"

set +e
run_mission_json "pause_already_paused" pause --mission-file "${MISSION_PATH}" --reason "double_pause"
PAUSE_RC=$?
set -e

if [[ "${PAUSE_RC}" -ne 0 ]]; then
  # Expected: non-zero exit for invalid transition
  emit_log "passed" "pause_failure_injection" "expected_error" "none" \
    "$(basename "${LAST_STDOUT_FILE}")" "pause from Paused state correctly rejected with non-zero exit"
else
  # Some implementations return ok=true with no_op=true instead
  if jq -e '.ok == true and .data.no_op == true' "${LAST_STDOUT_FILE}" >/dev/null 2>&1; then
    emit_log "passed" "pause_failure_injection" "expected_noop" "none" \
      "$(basename "${LAST_STDOUT_FILE}")" "pause from Paused state returned no-op"
  else
    emit_log "failed" "pause_failure_injection" "unexpected_success" "invalid_transition_allowed" \
      "$(basename "${LAST_STDOUT_FILE}")" "pause from Paused state should fail or no-op"
    echo "Failure injection check failed: pause from Paused should error or no-op" >&2
    exit 1
  fi
fi

# ── Scenario 6: Resume mission (Paused → Running) ─────────────────────
emit_log "running" "mission_resume_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission resume"

run_mission_json "resume_nominal" resume --mission-file "${MISSION_PATH}"
assert_jq_true \
  "resume_nominal" \
  '.ok == true and .data.command == "resume" and .data.lifecycle_state == "Running"' \
  "validate resume transition Paused → Running"

# Verify persisted
run_mission_json "status_after_resume" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "status_after_resume" \
  '.ok == true and .data.lifecycle_state == "Running"' \
  "validate mission file persisted Running state after resume"

# ── Scenario 7: Abort mission (Running → Cancelled) ───────────────────
emit_log "running" "mission_abort_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission abort --reason operator_cancel"

run_mission_json "abort_nominal" abort --mission-file "${MISSION_PATH}" --reason "operator_cancel"
assert_jq_true \
  "abort_nominal" \
  '.ok == true and .data.command == "abort" and .data.lifecycle_state == "Cancelled"' \
  "validate abort transition Running → Cancelled"

# ── Scenario 8: Failure injection — resume from Cancelled ─────────────
emit_log "running" "resume_failure_injection" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission resume (from Cancelled — expect error)"

set +e
run_mission_json "resume_from_cancelled" resume --mission-file "${MISSION_PATH}"
RESUME_RC=$?
set -e

if [[ "${RESUME_RC}" -ne 0 ]]; then
  emit_log "passed" "resume_failure_injection" "expected_error" "none" \
    "$(basename "${LAST_STDOUT_FILE}")" "resume from Cancelled state correctly rejected"
else
  if jq -e '.ok == true and .data.no_op == true' "${LAST_STDOUT_FILE}" >/dev/null 2>&1; then
    emit_log "passed" "resume_failure_injection" "expected_noop" "none" \
      "$(basename "${LAST_STDOUT_FILE}")" "resume from Cancelled state returned no-op"
  else
    emit_log "failed" "resume_failure_injection" "unexpected_success" "invalid_transition_allowed" \
      "$(basename "${LAST_STDOUT_FILE}")" "resume from Cancelled should fail or no-op"
    echo "Failure injection check failed: resume from Cancelled should error or no-op" >&2
    exit 1
  fi
fi

# ── Scenario 9: Recovery — create fresh mission and run full cycle ─────
emit_log "running" "recovery_full_cycle" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "fresh mission: status → pause → resume → abort (full lifecycle)"

# Write fresh Running mission
cat >"${MISSION_PATH}" <<'JSON'
{
  "mission_version": 1,
  "mission_id": "m-e2e-recovery",
  "title": "Recovery Cycle Mission",
  "workspace_id": "ws-e2e-test",
  "ownership": {
    "planner": "e2e-planner",
    "dispatcher": "e2e-dispatcher",
    "operator": "e2e-operator"
  },
  "lifecycle_state": "Running",
  "created_at_ms": 1704200000000
}
JSON

run_mission_json "recovery_status" status --mission-file "${MISSION_PATH}"
assert_jq_true "recovery_status" '.ok == true and .data.lifecycle_state == "Running"' \
  "recovery: initial Running state"

run_mission_json "recovery_pause" pause --mission-file "${MISSION_PATH}" --reason "recovery_test"
assert_jq_true "recovery_pause" '.ok == true and .data.lifecycle_state == "Paused"' \
  "recovery: pause transition"

run_mission_json "recovery_resume" resume --mission-file "${MISSION_PATH}"
assert_jq_true "recovery_resume" '.ok == true and .data.lifecycle_state == "Running"' \
  "recovery: resume transition"

run_mission_json "recovery_abort" abort --mission-file "${MISSION_PATH}" --reason "recovery_done"
assert_jq_true "recovery_abort" '.ok == true and .data.lifecycle_state == "Cancelled"' \
  "recovery: abort transition completes full cycle"

# ── Scenario 10: Missing file error path ──────────────────────────────
emit_log "running" "missing_file_error" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission status with nonexistent mission file"

set +e
run_mission_json "missing_file" status --mission-file "${WORKSPACE_DIR}/.ft/mission/nonexistent.json"
MISSING_RC=$?
set -e

if [[ "${MISSING_RC}" -ne 0 ]]; then
  emit_log "passed" "missing_file_error" "expected_error" "none" \
    "$(basename "${LAST_STDOUT_FILE}")" "missing mission file correctly returned error exit code"
else
  emit_log "failed" "missing_file_error" "unexpected_success" "missing_file_no_error" \
    "$(basename "${LAST_STDOUT_FILE}")" "missing mission file should return non-zero exit"
  echo "Error path check failed: missing file should return error" >&2
  exit 1
fi

# ── Cleanup ────────────────────────────────────────────────────────────
rm -rf "${WORKSPACE_DIR}"

emit_log \
  "passed" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "validated status, explain, pause, resume, abort, failure injection, recovery, and error paths"

echo "Mission MCP tools e2e passed (10 scenarios). Logs: ${LOG_FILE#${ROOT_DIR}/}"
