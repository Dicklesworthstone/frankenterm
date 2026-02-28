#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_interfaces"
CORRELATION_ID="ft-1i2ge.8.8-${RUN_ID}"
TARGET_DIR="target-rch-mission-tx-interfaces-${RUN_ID}"
LOG_FILE="${LOG_DIR}/mission_tx_interfaces_${RUN_ID}.jsonl"
STDOUT_BASENAME="mission_tx_interfaces_${RUN_ID}"
export TMPDIR="/tmp"
WORKSPACE_DIR="${ROOT_DIR}/tests/e2e/tmp/${SCENARIO_ID}_${RUN_ID}"
CONTRACT_PATH="${WORKSPACE_DIR}/.ft/mission/tx-active.json"
RCH_CHECK_LOG="${LOG_DIR}/${STDOUT_BASENAME}.rch_check.log"
RCH_STATUS_JSON="${LOG_DIR}/${STDOUT_BASENAME}.rch_status.json"
RCH_STATUS_ERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_status.stderr.log"
RCH_PROBE_JSON="${LOG_DIR}/${STDOUT_BASENAME}.rch_probe.json"
RCH_PROBE_ERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_probe.stderr.log"
RCH_SMOKE_STDOUT="${LOG_DIR}/${STDOUT_BASENAME}.rch_smoke.stdout.log"
RCH_SMOKE_STDERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_smoke.stderr.log"

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
    --arg component "mission_tx_interfaces.e2e" \
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
  "validate ft robot tx interfaces and failure/recovery semantics"

mkdir -p "${WORKSPACE_DIR}"

rch_check_rc=0
if ! rch check >"${RCH_CHECK_LOG}" 2>&1; then
  rch_check_rc=$?
  emit_log \
    "running" \
    "preflight_rch_check" \
    "rch_check_nonzero" \
    "none" \
    "$(basename "${RCH_CHECK_LOG}")" \
    "rch check returned non-zero; evaluating status/probe artifacts for final gate decision"
else
  emit_log \
    "passed" \
    "preflight_rch_check" \
    "rch_check_ok" \
    "none" \
    "$(basename "${RCH_CHECK_LOG}")" \
    "rch check passed"
fi

if ! rch status --workers --jobs --json >"${RCH_STATUS_JSON}" 2>"${RCH_STATUS_ERR}"; then
  emit_log \
    "failed" \
    "preflight_rch_status" \
    "rch_status_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_STATUS_ERR}")" \
    "unable to collect rch status snapshot"
  exit 1
fi

if ! rch workers probe --all --json >"${RCH_PROBE_JSON}" 2>"${RCH_PROBE_ERR}"; then
  emit_log \
    "failed" \
    "preflight_rch_probe" \
    "rch_probe_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_PROBE_ERR}")" \
    "rch workers probe invocation failed"
  exit 1
fi

workers_healthy="$(jq -r '.data.daemon.workers_healthy // 0' "${RCH_STATUS_JSON}" 2>/dev/null || echo 0)"
workers_reachable="$(jq -r '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${RCH_PROBE_JSON}" 2>/dev/null || echo 0)"

if [[ "${workers_reachable}" == "0" ]]; then
  if [[ "${workers_healthy}" != "0" ]]; then
    emit_log \
      "failed" \
      "preflight_rch_probe" \
      "rch_health_probe_mismatch" \
      "RCH-E101" \
      "$(basename "${RCH_PROBE_JSON}")" \
      "rch status reported healthy workers but probe found zero reachable workers"
  else
    emit_log \
      "failed" \
      "preflight_rch_probe" \
      "rch_workers_unreachable" \
      "remote_worker_unavailable" \
      "$(basename "${RCH_PROBE_JSON}")" \
      "no healthy rch workers available"
  fi
  exit 1
fi

if [[ "${rch_check_rc}" -ne 0 ]]; then
  emit_log \
    "running" \
    "preflight_rch_check" \
    "rch_check_nonzero_but_probe_passed" \
    "none" \
    "$(basename "${RCH_CHECK_LOG}")" \
    "continuing because probe reported reachable workers despite rch check non-zero"
fi

set +e
(
  cd "${ROOT_DIR}"
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" cargo --version
) >"${RCH_SMOKE_STDOUT}" 2>"${RCH_SMOKE_STDERR}"
smoke_rc=$?
set -e
if grep -Fq "[RCH] local" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
  emit_log \
    "failed" \
    "preflight_rch_smoke" \
    "rch_local_fallback_detected" \
    "remote_exec_failed_local_fallback" \
    "$(basename "${RCH_SMOKE_STDERR}")" \
    "remote smoke command fell back to local execution"
  exit 1
fi
if [[ "${smoke_rc}" -ne 0 ]]; then
  emit_log \
    "failed" \
    "preflight_rch_smoke" \
    "rch_smoke_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_SMOKE_STDERR}")" \
    "rch smoke command failed before tx suite execution"
  exit 1
fi

mkdir -p "$(dirname "${CONTRACT_PATH}")"
cat >"${CONTRACT_PATH}" <<'JSON'
{
  "tx_version": 1,
  "intent": {
    "tx_id": "tx:mission-interface-e2e",
    "requested_by": "dispatcher",
    "summary": "mission tx interface e2e contract",
    "correlation_id": "mission-tx-interfaces-corr",
    "created_at_ms": 1704200000000
  },
  "plan": {
    "plan_id": "tx-plan:mission-interface-e2e",
    "tx_id": "tx:mission-interface-e2e",
    "steps": [
      {
        "step_id": "tx-step:1",
        "ordinal": 1,
        "action": {
          "SendText": {
            "pane_id": 7,
            "text": "/do-step-1",
            "paste_mode": false
          }
        }
      },
      {
        "step_id": "tx-step:2",
        "ordinal": 2,
        "action": {
          "SendText": {
            "pane_id": 8,
            "text": "/do-step-2",
            "paste_mode": true
          }
        }
      }
    ],
    "preconditions": [
      {
        "type": "prompt_active",
        "pane_id": 7
      }
    ],
    "compensations": [
      {
        "for_step_id": "tx-step:1",
        "action": {
          "SendText": {
            "pane_id": 7,
            "text": "/undo-step-1",
            "paste_mode": false
          }
        }
      },
      {
        "for_step_id": "tx-step:2",
        "action": {
          "SendText": {
            "pane_id": 8,
            "text": "/undo-step-2",
            "paste_mode": true
          }
        }
      }
    ]
  },
  "lifecycle_state": "planned",
  "outcome": {
    "kind": "pending"
  },
  "receipts": []
}
JSON

run_robot_json() {
  local label="$1"
  shift
  local decision_path="$label"
  local stdout_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stdout.json"
  local stderr_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stderr.log"
  local detected_local_fallback=0

  set +e
  (
    cd "${ROOT_DIR}"
    rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" \
      cargo run -q -p frankenterm --bin ft -- \
      --workspace "${WORKSPACE_DIR}" \
      robot \
      --format json \
      "$@"
  ) >"${stdout_file}" 2>"${stderr_file}" &
  local cmd_pid=$!

  while kill -0 "${cmd_pid}" >/dev/null 2>&1; do
    if grep -Fq "[RCH] local" "${stdout_file}" "${stderr_file}" 2>/dev/null; then
      detected_local_fallback=1
      kill "${cmd_pid}" >/dev/null 2>&1 || true
      pkill -TERM -P "${cmd_pid}" >/dev/null 2>&1 || true
      break
    fi
    sleep 1
  done

  wait "${cmd_pid}"
  local rc=$?
  set -e

  LAST_STDOUT_FILE="${stdout_file}"
  LAST_STDERR_FILE="${stderr_file}"
  if [[ "${detected_local_fallback}" -eq 1 ]] || grep -Fq "[RCH] local" "${stdout_file}" "${stderr_file}"; then
    emit_log "failed" "${decision_path}" "rch_local_fallback_detected" "remote_exec_failed_local_fallback" \
      "$(basename "${stderr_file}")" "rch emitted local fallback marker"
    echo "RCH local fallback detected for ${label}; refusing local execution" >&2
    echo "Stdout: ${stdout_file}" >&2
    echo "Stderr: ${stderr_file}" >&2
    exit 1
  fi
  if [[ "${rc}" -ne 0 ]]; then
    emit_log "failed" "${decision_path}" "command_failed" "robot_command_failed" \
      "$(basename "${stderr_file}")" "robot tx command exited non-zero"
    echo "Command failed for ${label} (rc=${rc})" >&2
    echo "Stdout: ${stdout_file}" >&2
    echo "Stderr: ${stderr_file}" >&2
    exit 1
  fi
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

emit_log "running" "show_plan_surface" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx show + tx plan + tx show --include-contract"

run_robot_json "show_nominal" tx show
assert_jq_true \
  "show_nominal" \
  '.ok == true and .data.tx_id == "tx:mission-interface-e2e" and .data.step_count == 2 and .data.receipt_count == 0' \
  "validate tx show nominal summary"

SHOW_SIGNATURE_1="$(jq -c '{tx_id:.data.tx_id,plan_id:.data.plan_id,lifecycle_state:.data.lifecycle_state,step_count:.data.step_count,precondition_count:.data.precondition_count,compensation_count:.data.compensation_count,receipt_count:.data.receipt_count,legal_transitions:.data.legal_transitions}' "${LAST_STDOUT_FILE}")"

run_robot_json "plan_nominal" tx plan
assert_jq_true \
  "plan_nominal" \
  '.ok == true and .data.tx_id == "tx:mission-interface-e2e" and .data.step_count == 2 and .data.precondition_count == 1 and .data.compensation_count == 2' \
  "validate tx plan summary counts"

run_robot_json "show_include_contract" tx show --include-contract
assert_jq_true \
  "show_include_contract" \
  '.ok == true and .data.contract != null and .data.contract.intent.tx_id == "tx:mission-interface-e2e"' \
  "validate tx show includes full contract payload"

SHOW_SIGNATURE_2="$(jq -c '{tx_id:.data.tx_id,plan_id:.data.plan_id,lifecycle_state:.data.lifecycle_state,step_count:.data.step_count,precondition_count:.data.precondition_count,compensation_count:.data.compensation_count,receipt_count:.data.receipt_count,legal_transitions:.data.legal_transitions}' "${LAST_STDOUT_FILE}")"
if [[ "${SHOW_SIGNATURE_1}" != "${SHOW_SIGNATURE_2}" ]]; then
  emit_log "failed" "determinism_check" "signature_mismatch" "repeat_run_instability" \
    "$(basename "${LAST_STDOUT_FILE}")" "tx show signatures diverged across repeat run"
  echo "Determinism check failed: show signatures differ" >&2
  exit 1
fi
emit_log "passed" "determinism_check" "repeat_run_stable" "none" \
  "$(basename "${LAST_STDOUT_FILE}")" "tx show signatures stable across repeat run"

emit_log "running" "run_failure_injection" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx run --fail-step tx-step:2"

run_robot_json "run_fail_step" tx run --fail-step tx-step:2
assert_jq_true \
  "run_fail_step" \
  '.ok == true and .data.prepare_report.outcome == "all_ready" and .data.commit_report.outcome == "partial_failure" and .data.compensation_report.outcome == "fully_rolled_back" and .data.final_state == "rolled_back"' \
  "validate run failure-injection path and auto-compensation"

emit_log "running" "error_contract_surface" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx run --fail-step tx-step:missing"

run_robot_json "run_invalid_fail_step" tx run --fail-step tx-step:missing
assert_jq_true \
  "run_invalid_fail_step" \
  '.ok == false and .error_code == "robot.invalid_args"' \
  "validate stable error envelope for invalid fail-step input"

emit_log "running" "rollback_failure_and_recovery" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx rollback fail + recovery run"

run_robot_json "rollback_fail_comp" tx rollback --fail-compensation-for-step tx-step:1
assert_jq_true \
  "rollback_fail_comp" \
  '.ok == true and .data.compensation_report.outcome == "compensation_failed" and .data.final_state == "failed"' \
  "validate rollback failure-injection path"

run_robot_json "rollback_recovery" tx rollback
assert_jq_true \
  "rollback_recovery" \
  '.ok == true and .data.compensation_report.outcome == "fully_rolled_back" and .data.final_state == "rolled_back"' \
  "validate rollback recovery path"

emit_log \
  "passed" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "validated nominal, edge, failure injection, and recovery paths for ft robot tx interfaces"

echo "Mission tx interfaces e2e passed. Logs: ${LOG_FILE#"${ROOT_DIR}"/}"
