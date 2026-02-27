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
WORKSPACE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ft-mission-tx-interfaces.XXXXXX")"
CONTRACT_PATH="${WORKSPACE_DIR}/.ft/mission/tx-active.json"

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
  local stdout_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stdout.json"
  local stderr_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}.stderr.log"

  set +e
  (
    cd "${ROOT_DIR}"
    rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" \
      cargo run -q -p frankenterm --bin ft -- \
      --workspace "${WORKSPACE_DIR}" \
      robot \
      --format json \
      "$@"
  ) >"${stdout_file}" 2>"${stderr_file}"
  local rc=$?
  set -e

  LAST_STDOUT_FILE="${stdout_file}"
  LAST_STDERR_FILE="${stderr_file}"
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

echo "Mission tx interfaces e2e passed. Logs: ${LOG_FILE#${ROOT_DIR}/}"
