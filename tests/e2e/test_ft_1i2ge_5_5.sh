#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_1i2ge_5_5_operator_explain_report"
CORRELATION_ID="ft-1i2ge.5.5-${RUN_ID}"
TARGET_DIR="target-rch-ft-1i2ge-5-5-${RUN_ID}"
LOG_FILE="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.jsonl"
PROBE_FILE="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.probe.log"
CHECK_FILE="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.check.log"
STATUS_FILE="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.status.json"
WORKSPACE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ft-1i2ge-5-5.XXXXXX")"
MISSION_PATH="${WORKSPACE_DIR}/.ft/mission/active.json"
LAST_STDOUT_FILE=""
LAST_STDERR_FILE=""
LAST_RC=0
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
    --arg component "mission_operator_view.e2e" \
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
  echo "stdout: ${LAST_STDOUT_FILE}" >&2
  echo "stderr: ${LAST_STDERR_FILE}" >&2
  exit 1
}

run_mission_json() {
  local label="$1"
  shift
  local stdout_file="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.${label}.stdout.json"
  local stderr_file="${LOG_DIR}/ft_1i2ge_5_5_${RUN_ID}.${label}.stderr.log"

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

  if grep -q "\[RCH\] local" "${stdout_file}" "${stderr_file}" 2>/dev/null; then
    emit_log \
      "failed" \
      "execution_preflight" \
      "rch_local_fallback_detected" \
      "RCH-LOCAL-FALLBACK" \
      "$(basename "${stderr_file}")" \
      "local fallback detected for label=${label}"
    exit 3
  fi

  LAST_STDOUT_FILE="${stdout_file}"
  LAST_STDERR_FILE="${stderr_file}"
  LAST_RC="${rc}"
  return "${rc}"
}

write_baseline_mission() {
  mkdir -p "$(dirname "${MISSION_PATH}")"
  cat > "${MISSION_PATH}" <<'JSON'
{
  "mission_version": 1,
  "mission_id": "mission:e2e-operator-views",
  "title": "Operator explain/report contract mission",
  "workspace_id": "ws-e2e-operator-views",
  "ownership": {
    "planner": "planner-agent",
    "dispatcher": "dispatcher-agent",
    "operator": "operator-human"
  },
  "lifecycle_state": "running",
  "provenance": {
    "bead_id": "ft-1i2ge.5.5",
    "thread_id": "ft-1i2ge.5.5",
    "source_command": "ft mission plan",
    "source_sha": "deadbeef"
  },
  "created_at_ms": 1704200000000,
  "updated_at_ms": 1704200000300,
  "candidates": [
    {
      "candidate_id": "candidate:ready",
      "requested_by": "planner",
      "action": {
        "type": "send_text",
        "pane_id": 7,
        "text": "/retry",
        "paste_mode": false
      },
      "rationale": "retry the active pane",
      "score": 0.91,
      "created_at_ms": 1704200000100
    },
    {
      "candidate_id": "candidate:blocked",
      "requested_by": "planner",
      "action": {
        "type": "acquire_lock",
        "lock_name": "mission-lock",
        "timeout_ms": 1000
      },
      "rationale": "serialize conflicting operations",
      "score": 0.62,
      "created_at_ms": 1704200000200
    },
    {
      "candidate_id": "candidate:failed",
      "requested_by": "planner",
      "action": {
        "type": "wait_for",
        "pane_id": 7,
        "condition": {
          "type": "pane_idle",
          "pane_id": 7,
          "idle_threshold_ms": 2500
        },
        "timeout_ms": 20000
      },
      "rationale": "wait for stability after retry",
      "score": 0.55,
      "created_at_ms": 1704200000250
    }
  ],
  "assignments": [
    {
      "assignment_id": "assignment:ready",
      "candidate_id": "candidate:ready",
      "assigned_by": "dispatcher",
      "assignee": "executor-1",
      "approval_state": {
        "state": "approved",
        "approved_by": "operator-human",
        "approved_at_ms": 1704200000150,
        "approval_code_hash": "sha256:ready"
      },
      "created_at_ms": 1704200000140,
      "updated_at_ms": 1704200000150
    },
    {
      "assignment_id": "assignment:blocked",
      "candidate_id": "candidate:blocked",
      "assigned_by": "dispatcher",
      "assignee": "executor-2",
      "approval_state": {
        "state": "denied",
        "denied_by": "operator-human",
        "denied_at_ms": 1704200000220,
        "reason_code": "policy_denied"
      },
      "created_at_ms": 1704200000210,
      "updated_at_ms": 1704200000220
    },
    {
      "assignment_id": "assignment:failed",
      "candidate_id": "candidate:failed",
      "assigned_by": "dispatcher",
      "assignee": "executor-3",
      "approval_state": {
        "state": "approved",
        "approved_by": "operator-human",
        "approved_at_ms": 1704200000275,
        "approval_code_hash": "sha256:failed"
      },
      "outcome": {
        "kind": "failed",
        "reason_code": "dispatch_error",
        "error_code": "FTM1005",
        "completed_at_ms": 1704200000300
      },
      "created_at_ms": 1704200000260,
      "updated_at_ms": 1704200000300
    }
  ]
}
JSON
}

write_recovery_mission() {
  mkdir -p "$(dirname "${MISSION_PATH}")"
  cat > "${MISSION_PATH}" <<'JSON'
{
  "mission_version": 1,
  "mission_id": "mission:e2e-operator-views-recovery",
  "title": "Operator explain/report recovery mission",
  "workspace_id": "ws-e2e-operator-views",
  "ownership": {
    "planner": "planner-agent",
    "dispatcher": "dispatcher-agent",
    "operator": "operator-human"
  },
  "lifecycle_state": "running",
  "provenance": {
    "bead_id": "ft-1i2ge.5.5",
    "thread_id": "ft-1i2ge.5.5",
    "source_command": "ft mission plan",
    "source_sha": "cafebabe"
  },
  "created_at_ms": 1704200100000,
  "updated_at_ms": 1704200100200,
  "candidates": [
    {
      "candidate_id": "candidate:healthy-active",
      "requested_by": "planner",
      "action": {
        "type": "send_text",
        "pane_id": 8,
        "text": "/continue",
        "paste_mode": false
      },
      "rationale": "continue healthy path",
      "score": 0.78,
      "created_at_ms": 1704200100100
    },
    {
      "candidate_id": "candidate:healthy-outcome",
      "requested_by": "planner",
      "action": {
        "type": "send_text",
        "pane_id": 8,
        "text": "echo ok",
        "paste_mode": false
      },
      "rationale": "record successful completion",
      "score": 0.66,
      "created_at_ms": 1704200100120
    }
  ],
  "assignments": [
    {
      "assignment_id": "assignment:healthy-active",
      "candidate_id": "candidate:healthy-active",
      "assigned_by": "dispatcher",
      "assignee": "executor-4",
      "approval_state": {
        "state": "approved",
        "approved_by": "operator-human",
        "approved_at_ms": 1704200100150,
        "approval_code_hash": "sha256:ok"
      },
      "created_at_ms": 1704200100140,
      "updated_at_ms": 1704200100150
    },
    {
      "assignment_id": "assignment:healthy-outcome",
      "candidate_id": "candidate:healthy-outcome",
      "assigned_by": "dispatcher",
      "assignee": "executor-5",
      "approval_state": {
        "state": "approved",
        "approved_by": "operator-human",
        "approved_at_ms": 1704200100160,
        "approval_code_hash": "sha256:ok2"
      },
      "outcome": {
        "kind": "success",
        "reason_code": "dispatch_executed",
        "completed_at_ms": 1704200100200
      },
      "created_at_ms": 1704200100155,
      "updated_at_ms": 1704200100200
    }
  ]
}
JSON
}

emit_log \
  "started" \
  "script_init" \
  "none" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "mission operator explain/report e2e started"

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required for structured logging" >&2
  exit 1
fi
if ! command -v rch >/dev/null 2>&1; then
  echo "rch is required; refusing local cargo execution" >&2
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch check
) >"${CHECK_FILE}" 2>&1
check_status=$?
set -e
if [[ ${check_status} -ne 0 ]]; then
  emit_log "failed" "execution_preflight" "rch_check_failed" "rch_health_check_failed" \
    "$(basename "${CHECK_FILE}")" "rch check failed; refusing local fallback"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  rch workers probe --all --json
) >"${PROBE_FILE}" 2>&1
probe_status=$?
set -e
probe_reachable=0
if [[ ${probe_status} -eq 0 ]]; then
  probe_reachable=$(jq -r '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${PROBE_FILE}" 2>/dev/null || echo 0)
fi
if [[ ${probe_status} -ne 0 ]] || [[ "${probe_reachable}" -lt 1 ]]; then
  set +e
  (
    cd "${ROOT_DIR}"
    rch --json status --workers --jobs
  ) >"${STATUS_FILE}" 2>&1
  status_rc=$?
  set -e
  if [[ ${status_rc} -ne 0 ]]; then
    emit_log "failed" "execution_preflight" "rch_status_failed" "remote_worker_unavailable" \
      "$(basename "${STATUS_FILE}")" "rch status fallback failed after probe failure"
    exit 1
  fi
  emit_log "failed" "execution_preflight" "rch_workers_unhealthy" "remote_worker_unavailable" \
    "$(basename "${PROBE_FILE}")" "rch workers probe/status failed; refusing local fallback"
  exit 1
fi

emit_log \
  "running" \
  "execution_preflight" \
  "rch_workers_healthy" \
  "none" \
  "$(basename "${PROBE_FILE}")" \
  "offloading all mission command checks through rch workers"

write_baseline_mission

emit_log "running" "status_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission status baseline"
run_mission_json "status_nominal" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "status_nominal" \
  '.ok == true and (.data.operator_view.active_decisions|length) == 2 and (.data.operator_view.blocked_work|length) == 1 and (.data.operator_view.recent_outcomes|length) == 1 and .data.operator_view.degraded_state.is_degraded == true' \
  "status output includes active decisions, blocked work, recent outcomes, and explicit degraded flag"

STATUS_SIG_1="$(jq -c '{degraded:.data.operator_view.degraded_state.code,active:(.data.operator_view.active_decisions|length),blocked:(.data.operator_view.blocked_work|length),recent:(.data.operator_view.recent_outcomes|length)}' "${LAST_STDOUT_FILE}")"
run_mission_json "status_nominal_repeat" status --mission-file "${MISSION_PATH}"
STATUS_SIG_2="$(jq -c '{degraded:.data.operator_view.degraded_state.code,active:(.data.operator_view.active_decisions|length),blocked:(.data.operator_view.blocked_work|length),recent:(.data.operator_view.recent_outcomes|length)}' "${LAST_STDOUT_FILE}")"
if [[ "${STATUS_SIG_1}" != "${STATUS_SIG_2}" ]]; then
  emit_log "failed" "status_determinism" "signature_mismatch" "repeat_run_instability" \
    "$(basename "${LAST_STDOUT_FILE}")" "status signature diverged across repeat run"
  exit 1
fi
emit_log "passed" "status_determinism" "repeat_run_stable" "none" \
  "$(basename "${LAST_STDOUT_FILE}")" "status signature stable across repeat run"

emit_log "running" "explain_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission explain baseline"
run_mission_json "explain_nominal" explain --mission-file "${MISSION_PATH}"
assert_jq_true \
  "explain_nominal" \
  '.ok == true and (.data.decision_provenance|length) >= 1 and .data.operator_view.degraded_state.code != null' \
  "explain output includes decision_provenance traces and degraded-state code"

emit_log "running" "explain_assignment_nominal" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission explain --assignment-id assignment:ready"
run_mission_json "explain_assignment_nominal" explain --mission-file "${MISSION_PATH}" --assignment-id "assignment:ready"
assert_jq_true \
  "explain_assignment_nominal" \
  '.ok == true and .data.assignment_context.assignment_id == "assignment:ready" and .data.assignment_context.dispatch_contract != null and .data.assignment_context.target != null and .data.assignment_context.dry_run_execution != null and .data.assignment_context.decision_path != null' \
  "assignment explain includes dispatch contract/target/dry-run and decision path"

emit_log "running" "failure_injection_missing_assignment" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft mission explain with nonexistent assignment"
set +e
run_mission_json "explain_missing_assignment" explain --mission-file "${MISSION_PATH}" --assignment-id "assignment:missing"
missing_rc=$?
set -e
if [[ ${missing_rc} -eq 0 ]]; then
  emit_log "failed" "failure_injection_missing_assignment" "unexpected_success" "missing_assignment_not_rejected" \
    "$(basename "${LAST_STDOUT_FILE}")" "missing assignment should return non-zero"
  exit 1
fi
if ! jq -e '.ok == false and .error_code == "mission.assignment_not_found"' "${LAST_STDOUT_FILE}" >/dev/null 2>&1; then
  emit_log "failed" "failure_injection_missing_assignment" "error_code_mismatch" "unexpected_error_contract" \
    "$(basename "${LAST_STDOUT_FILE}")" "missing assignment error_code must be mission.assignment_not_found"
  exit 1
fi
emit_log "passed" "failure_injection_missing_assignment" "expected_error" "none" \
  "$(basename "${LAST_STDOUT_FILE}")" "missing assignment produced expected error envelope"

emit_log "running" "degraded_blocked_state" "state_mutation" "none" \
  "$(basename "${MISSION_PATH}")" "set mission lifecycle_state to blocked"
tmp_blocked="${MISSION_PATH}.blocked.tmp"
jq '.lifecycle_state = "blocked"' "${MISSION_PATH}" > "${tmp_blocked}"
mv "${tmp_blocked}" "${MISSION_PATH}"
run_mission_json "status_blocked" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "degraded_blocked_state" \
  '.ok == true and .data.operator_view.degraded_state.is_degraded == true and .data.operator_view.degraded_state.code == "lifecycle_blocked" and (.data.operator_view.degraded_state.operator_action | length) > 0' \
  "blocked lifecycle emits explicit non-ambiguous degraded state and operator action"

emit_log "running" "recovery_path" "state_recovery" "none" \
  "$(basename "${MISSION_PATH}")" "rewrite mission fixture into healthy recovery state"
write_recovery_mission
run_mission_json "status_recovery" status --mission-file "${MISSION_PATH}"
assert_jq_true \
  "recovery_path" \
  '.ok == true and .data.operator_view.degraded_state.is_degraded == false and (.data.operator_view.blocked_work|length) == 0 and (.data.operator_view.active_decisions|length) >= 1 and (.data.operator_view.recent_outcomes|length) >= 1' \
  "recovery path clears degraded ambiguity and preserves active/recent operator visibility"

emit_log \
  "passed" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "validated operator report/explain nominal, failure injection, blocked clarity, determinism, and recovery contracts"

echo "ft-1i2ge.5.5 e2e passed. Logs: ${LOG_FILE_REL}"
