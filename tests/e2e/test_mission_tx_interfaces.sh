#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="mission_tx_interfaces"
CORRELATION_ID="ft-1i2ge.8.8-${RUN_ID}"
TARGET_DIR="${MISSION_TX_RCH_TARGET_DIR:-target-rch-mission-tx-interfaces}"
REMOTE_TMPDIR="${MISSION_TX_RCH_REMOTE_TMPDIR:-/tmp/rch-mission-tx-interfaces}"
LOG_FILE="${LOG_DIR}/mission_tx_interfaces_${RUN_ID}.jsonl"
STDOUT_BASENAME="mission_tx_interfaces_${RUN_ID}"
export TMPDIR="/tmp"
export RCH_DAEMON_TIMEOUT_MS="${RCH_DAEMON_TIMEOUT_MS:-120000}"
WORKSPACE_DIR="${ROOT_DIR}/tests/e2e/tmp/${SCENARIO_ID}_${RUN_ID}"
WORKSPACE_REL_DIR="tests/e2e/tmp/${SCENARIO_ID}_${RUN_ID}"
CONTRACT_PATH="${WORKSPACE_DIR}/.ft/mission/tx-active.json"
RCH_CHECK_LOG="${LOG_DIR}/${STDOUT_BASENAME}.rch_check.log"
RCH_STATUS_JSON="${LOG_DIR}/${STDOUT_BASENAME}.rch_status.json"
RCH_STATUS_ERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_status.stderr.log"
RCH_PROBE_JSON="${LOG_DIR}/${STDOUT_BASENAME}.rch_probe.json"
RCH_PROBE_ERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_probe.stderr.log"
RCH_SMOKE_STDOUT="${LOG_DIR}/${STDOUT_BASENAME}.rch_smoke.stdout.log"
RCH_SMOKE_STDERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_smoke.stderr.log"
RCH_DISK_STDOUT="${LOG_DIR}/${STDOUT_BASENAME}.rch_disk.stdout.log"
RCH_DISK_STDERR="${LOG_DIR}/${STDOUT_BASENAME}.rch_disk.stderr.log"
RCH_MIN_FREE_KB=2097152

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
  rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" TMPDIR="${REMOTE_TMPDIR}" cargo --version
) >"${RCH_SMOKE_STDOUT}" 2>"${RCH_SMOKE_STDERR}"
smoke_rc=$?
set -e
if grep -Fq "[RCH] local" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
  if grep -Eq "Project sync failed: rsync failed|rsync: \\[receiver\\].*\\.beads\\.?db" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_smoke" \
      "rch_sync_churn_artifacts" \
      "RCH-E106" \
      "$(basename "${RCH_SMOKE_STDERR}")" \
      "remote smoke command fell back local after rsync churn on volatile local artifacts"
    exit 1
  fi
  if grep -Fq "Failed to connect to" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_smoke" \
      "rch_worker_connect_failed" \
      "RCH-E100" \
      "$(basename "${RCH_SMOKE_STDERR}")" \
      "remote smoke command fell back local after SSH connectivity failure"
    exit 1
  fi
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
  if grep -Fq "No space left on device" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_smoke" \
      "remote_worker_disk_exhausted" \
      "RCH-E102" \
      "$(basename "${RCH_SMOKE_STDERR}")" \
      "remote smoke command failed due to disk exhaustion while validating TMPDIR"
    exit 1
  fi
  if grep -Fq "no workers with Rust installed" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_smoke" \
      "rch_workers_missing_rust" \
      "RCH-E103" \
      "$(basename "${RCH_SMOKE_STDERR}")" \
      "remote smoke command reported no workers with Rust installed"
    exit 1
  fi
  if grep -Fq "Failed to query daemon" "${RCH_SMOKE_STDOUT}" "${RCH_SMOKE_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_smoke" \
      "rch_daemon_unreachable" \
      "RCH-E104" \
      "$(basename "${RCH_SMOKE_STDERR}")" \
      "remote smoke command could not reach rch daemon"
    exit 1
  fi
  emit_log \
    "failed" \
    "preflight_rch_smoke" \
    "rch_smoke_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_SMOKE_STDERR}")" \
    "rch smoke command failed before tx suite execution"
  exit 1
fi

set +e
(
  cd "${ROOT_DIR}"
  # shellcheck disable=SC2016
  rch exec -- env TMPDIR="${REMOTE_TMPDIR}" bash -lc '
    set -euo pipefail
    mkdir -p "${TMPDIR}"
    tmp_free_kb="$(df -Pk "${TMPDIR}" | awk "NR==2 { print \$4 }")"
    workspace_free_kb="$(df -Pk "." | awk "NR==2 { print \$4 }")"
    printf "tmp_free_kb=%s\nworkspace_free_kb=%s\n" "${tmp_free_kb}" "${workspace_free_kb}"
  '
) >"${RCH_DISK_STDOUT}" 2>"${RCH_DISK_STDERR}"
disk_rc=$?
set -e
if grep -Fq "[RCH] local" "${RCH_DISK_STDOUT}" "${RCH_DISK_STDERR}"; then
  emit_log \
    "failed" \
    "preflight_rch_disk_capacity" \
    "rch_local_fallback_detected" \
    "remote_exec_failed_local_fallback" \
    "$(basename "${RCH_DISK_STDERR}")" \
    "remote disk-capacity preflight fell back to local execution"
  exit 1
fi
if [[ "${disk_rc}" -ne 0 ]]; then
  if grep -Fq "No space left on device" "${RCH_DISK_STDOUT}" "${RCH_DISK_STDERR}"; then
    emit_log \
      "failed" \
      "preflight_rch_disk_capacity" \
      "remote_worker_disk_exhausted" \
      "RCH-E102" \
      "$(basename "${RCH_DISK_STDERR}")" \
      "remote disk-capacity preflight failed with disk exhaustion"
    exit 1
  fi
  emit_log \
    "failed" \
    "preflight_rch_disk_capacity" \
    "rch_disk_preflight_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_DISK_STDERR}")" \
    "remote disk-capacity preflight command failed"
  exit 1
fi

tmp_free_kb="$(awk -F= '/^tmp_free_kb=/{print $2}' "${RCH_DISK_STDOUT}" | tail -n1)"
workspace_free_kb="$(awk -F= '/^workspace_free_kb=/{print $2}' "${RCH_DISK_STDOUT}" | tail -n1)"

if ! [[ "${tmp_free_kb}" =~ ^[0-9]+$ ]] || ! [[ "${workspace_free_kb}" =~ ^[0-9]+$ ]]; then
  emit_log \
    "failed" \
    "preflight_rch_disk_capacity" \
    "rch_disk_preflight_parse_failed" \
    "remote_worker_unavailable" \
    "$(basename "${RCH_DISK_STDOUT}")" \
    "failed to parse remote disk-capacity preflight output"
  exit 1
fi

if (( tmp_free_kb < RCH_MIN_FREE_KB || workspace_free_kb < RCH_MIN_FREE_KB )); then
  emit_log \
    "failed" \
    "preflight_rch_disk_capacity" \
    "remote_worker_disk_low" \
    "RCH-E105" \
    "$(basename "${RCH_DISK_STDOUT}")" \
    "remote free space below threshold: tmp_free_kb=${tmp_free_kb}, workspace_free_kb=${workspace_free_kb}, min_free_kb=${RCH_MIN_FREE_KB}"
  exit 1
fi

emit_log \
  "passed" \
  "preflight_rch_disk_capacity" \
  "rch_disk_capacity_ok" \
  "none" \
  "$(basename "${RCH_DISK_STDOUT}")" \
  "remote free space ok: tmp_free_kb=${tmp_free_kb}, workspace_free_kb=${workspace_free_kb}, min_free_kb=${RCH_MIN_FREE_KB}"

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
  local attempt=1

  while true; do
    local attempt_suffix=""
    if [[ "${attempt}" -gt 1 ]]; then
      attempt_suffix=".retry${attempt}"
    fi

    local stdout_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}${attempt_suffix}.stdout.json"
    local stderr_file="${LOG_DIR}/${STDOUT_BASENAME}.${label}${attempt_suffix}.stderr.log"
    local detected_local_fallback=0

    set +e
    (
      cd "${ROOT_DIR}"
      rch exec -- env CARGO_TARGET_DIR="${TARGET_DIR}" TMPDIR="${REMOTE_TMPDIR}" \
        cargo run -q -p frankenterm --bin ft -- \
        --workspace "${WORKSPACE_REL_DIR}" \
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
      local fallback_reason_code="rch_local_fallback_detected"
      local fallback_error_code="remote_exec_failed_local_fallback"
      local fallback_summary="rch emitted local fallback marker"
      if grep -Eq "Project sync failed: rsync failed|rsync: \\[receiver\\].*\\.beads\\.?db" "${stdout_file}" "${stderr_file}"; then
        fallback_reason_code="rch_sync_churn_artifacts"
        fallback_error_code="RCH-E106"
        fallback_summary="rch local fallback: rsync churn on volatile local artifacts"
      elif grep -Fq "Failed to connect to" "${stdout_file}" "${stderr_file}"; then
        fallback_reason_code="rch_worker_connect_failed"
        fallback_error_code="RCH-E100"
        fallback_summary="rch local fallback: SSH connectivity failure to selected worker"
      elif grep -Fq "Command timed out after" "${stdout_file}" "${stderr_file}"; then
        fallback_reason_code="rch_remote_timeout"
        fallback_error_code="RCH-E107"
        fallback_summary="rch local fallback: remote command timed out"
      elif grep -Fq "no workers with Rust installed" "${stdout_file}" "${stderr_file}"; then
        fallback_reason_code="rch_workers_missing_rust"
        fallback_error_code="RCH-E103"
        fallback_summary="rch local fallback: no workers with Rust installed"
      elif grep -Fq "Failed to query daemon" "${stdout_file}" "${stderr_file}"; then
        fallback_reason_code="rch_daemon_unreachable"
        fallback_error_code="RCH-E104"
        fallback_summary="rch local fallback: daemon query failed"
      fi

      if [[ "${attempt}" -eq 1 ]] && { [[ "${fallback_error_code}" == "RCH-E100" ]] || [[ "${fallback_error_code}" == "RCH-E103" ]] || [[ "${fallback_error_code}" == "RCH-E104" ]] || [[ "${fallback_error_code}" == "RCH-E106" ]] || [[ "${fallback_error_code}" == "RCH-E107" ]]; }; then
        local retry_status_json="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_status.json"
        local retry_status_err="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_status.stderr.log"
        local retry_probe_json="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_probe.json"
        local retry_probe_err="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_probe.stderr.log"
        local retry_daemon_restart_log="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_daemon_restart.log"
        local retry_daemon_restart_err="${LOG_DIR}/${STDOUT_BASENAME}.${label}.retry_preflight.rch_daemon_restart.stderr.log"
        local retry_status_rc=0
        local retry_probe_rc=0
        local retry_restart_rc=0

        if [[ "${fallback_error_code}" == "RCH-E104" ]]; then
          if ! rch daemon restart -y >"${retry_daemon_restart_log}" 2>"${retry_daemon_restart_err}"; then
            retry_restart_rc=$?
            emit_log "running" "${decision_path}" "rch_daemon_restart_failed" "none" \
              "$(basename "${retry_daemon_restart_err}")" \
              "retry remediation: daemon restart failed after ${fallback_reason_code}"
          else
            emit_log "running" "${decision_path}" "rch_daemon_restarted" "none" \
              "$(basename "${retry_daemon_restart_log}")" \
              "retry remediation: daemon restarted after ${fallback_reason_code}"
          fi
        fi

        if ! rch status --workers --jobs --json >"${retry_status_json}" 2>"${retry_status_err}"; then
          retry_status_rc=$?
        fi
        if ! rch workers probe --all --json >"${retry_probe_json}" 2>"${retry_probe_err}"; then
          retry_probe_rc=$?
        fi

        local retry_workers_healthy=0
        local retry_workers_reachable=0
        if [[ "${retry_status_rc}" -eq 0 ]]; then
          retry_workers_healthy="$(jq -r '.data.daemon.workers_healthy // 0' "${retry_status_json}" 2>/dev/null || echo 0)"
        fi
        if [[ "${retry_probe_rc}" -eq 0 ]]; then
          retry_workers_reachable="$(jq -r '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${retry_probe_json}" 2>/dev/null || echo 0)"
        fi

        if [[ "${retry_restart_rc}" -eq 0 ]] && [[ "${retry_status_rc}" -eq 0 ]] && [[ "${retry_probe_rc}" -eq 0 ]] && [[ "${retry_workers_healthy}" != "0" ]] && [[ "${retry_workers_reachable}" != "0" ]]; then
          emit_log "running" "${decision_path}" "rch_retry_preflight_recovered" "none" \
            "$(basename "${retry_probe_json}")" \
            "retry preflight passed after ${fallback_reason_code}; retrying once"
          attempt=$((attempt + 1))
          continue
        fi
      fi

      emit_log "failed" "${decision_path}" "${fallback_reason_code}" "${fallback_error_code}" \
        "$(basename "${stderr_file}")" "${fallback_summary}"
      echo "RCH local fallback detected for ${label}; refusing local execution" >&2
      echo "Stdout: ${stdout_file}" >&2
      echo "Stderr: ${stderr_file}" >&2
      exit 1
    fi

    if [[ "${rc}" -ne 0 ]]; then
      if grep -Fq "No space left on device" "${stdout_file}" "${stderr_file}"; then
        emit_log "failed" "${decision_path}" "remote_worker_disk_exhausted" "RCH-E102" \
          "$(basename "${stderr_file}")" \
          "robot tx command failed due to remote disk exhaustion (TMPDIR/CARGO target path)"
        echo "Remote worker disk exhaustion detected for ${label}" >&2
        echo "Stdout: ${stdout_file}" >&2
        echo "Stderr: ${stderr_file}" >&2
        exit 1
      fi
      if grep -Fq "Workspace path not writable" "${stdout_file}" "${stderr_file}"; then
        emit_log "failed" "${decision_path}" "workspace_path_unwritable" "FT-7010" \
          "$(basename "${stderr_file}")" \
          "robot tx command rejected non-writable workspace path on remote execution"
        echo "Workspace path not writable for ${label}" >&2
        echo "Stdout: ${stdout_file}" >&2
        echo "Stderr: ${stderr_file}" >&2
        exit 1
      fi
      emit_log "failed" "${decision_path}" "command_failed" "robot_command_failed" \
        "$(basename "${stderr_file}")" "robot tx command exited non-zero"
      echo "Command failed for ${label} (rc=${rc})" >&2
      echo "Stdout: ${stdout_file}" >&2
      echo "Stderr: ${stderr_file}" >&2
      exit 1
    fi

    break
  done
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
  '.ok == true and .data.prepare_report.outcome == "all_ready" and .data.commit_report.outcome == "partial_failure" and .data.commit_report.failure_boundary == "tx-step:2" and .data.commit_report.committed_count == 1 and .data.commit_report.failed_count == 1 and .data.commit_report.skipped_count == 0 and (.data.commit_report.receipts | length) == 2 and .data.compensation_report.outcome == "fully_rolled_back" and .data.compensation_report.compensated_count == 1 and .data.compensation_report.failed_count == 0 and .data.compensation_report.skipped_count == 0 and (.data.compensation_report.receipts | length) == 1 and .data.final_state == "compensated"' \
  "validate run failure-injection path and auto-compensation"

emit_log "running" "run_pause_surface" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx run --paused"

run_robot_json "run_paused" tx run --paused
assert_jq_true \
  "run_paused" \
  '.ok == true and .data.prepare_report.outcome == "all_ready" and .data.commit_report.outcome == "pause_suspended" and .data.commit_report.committed_count == 0 and .data.commit_report.failed_count == 0 and .data.commit_report.skipped_count == 2 and (.data.commit_report.receipts | length) == 2 and .data.compensation_report == null and .data.final_state == "committing"' \
  "validate paused commit path stays non-compensating and explicit"

emit_log "running" "run_safe_mode_surface" "command_execution" "none" \
  "$(basename "${LOG_FILE}")" "ft robot tx run --kill-switch safe-mode"

run_robot_json "run_safe_mode" tx run --kill-switch safe-mode
assert_jq_true \
  "run_safe_mode" \
  '.ok == true and .data.prepare_report.outcome == "all_ready" and .data.commit_report.outcome == "kill_switch_blocked" and .data.commit_report.committed_count == 0 and .data.commit_report.failed_count == 0 and .data.commit_report.skipped_count == 2 and (.data.commit_report.receipts | length) == 2 and .data.compensation_report == null and .data.final_state == "failed"' \
  "validate safe-mode kill-switch block path"

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
  '.ok == true and .data.compensation_report.outcome == "compensation_failed" and .data.compensation_report.compensated_count == 1 and .data.compensation_report.failed_count == 1 and .data.compensation_report.skipped_count == 0 and (.data.compensation_report.receipts | length) == 2 and .data.final_state == "failed"' \
  "validate rollback failure-injection path"

run_robot_json "rollback_recovery" tx rollback
assert_jq_true \
  "rollback_recovery" \
  '.ok == true and .data.compensation_report.outcome == "fully_rolled_back" and .data.compensation_report.compensated_count == 2 and .data.compensation_report.failed_count == 0 and .data.compensation_report.skipped_count == 0 and (.data.compensation_report.receipts | length) == 2 and .data.final_state == "compensated"' \
  "validate rollback recovery path"

emit_log \
  "passed" \
  "suite_complete" \
  "all_scenarios_passed" \
  "none" \
  "$(basename "${LOG_FILE}")" \
  "validated nominal, edge, failure injection, and recovery paths for ft robot tx interfaces"

echo "Mission tx interfaces e2e passed. Logs: ${LOG_FILE#"${ROOT_DIR}"/}"
