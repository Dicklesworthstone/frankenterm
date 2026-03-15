#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_4_1_unix_ipc"
CORRELATION_ID="ft-e34d9.10.4.1-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
VALIDATOR_PASS_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.validator.pass.json"
VALIDATOR_FAIL_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.validator.fail.json"
VALIDATOR_RECOVERY_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.validator.recovery.json"
WORKFLOW_SURFACE_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.workflow_surface.json"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft-e34d9-10-4-1-ipc}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""

source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "e34d9_10_4_1_unix_ipc"
ensure_rch_ready

emit_log() {
  local component="$1"
  local decision_path="$2"
  local input_summary="$3"
  local outcome="$4"
  local reason_code="$5"
  local error_code="$6"
  local artifact_path="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "${component}" \
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

run_step() {
  local label="$1"
  shift

  LAST_STEP_LOG="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_${label}.log"
  set +e
  "$@" 2>&1 | tee "${LAST_STEP_LOG}" | tee -a "${STDOUT_FILE}"
  local rc=${PIPESTATUS[0]}
  set -e
  return ${rc}
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    emit_log "preflight" "prereq_check" "missing:${cmd}" "failed" "missing_prerequisite" "E2E-PREREQ" "${cmd}"
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

ensure_no_local_fallback() {
  if grep -q "\[RCH\] local" "${LAST_STEP_LOG}"; then
    emit_log "validation" "rch_offload_policy" "remote_exec_required" "failed" "rch_fail_open_local_fallback" "RCH-LOCAL-FALLBACK" "$(basename "${LAST_STEP_LOG}")"
    echo "rch fell back to local execution; failing per offload-only policy" >&2
    exit 3
  fi
}

validate_ipc_contract() {
  local source_path="$1"
  local report_path="$2"
  python3 - "$source_path" "$report_path" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
text = source_path.read_text(encoding="utf-8")

required_tokens = [
    "self as compat_unix",
    "compat_unix::bind(",
    "compat_unix::connect(",
    "compat_unix::lines(compat_unix::buffered(reader))",
    "IPC_ACCEPT_POLL_INTERVAL",
]

missing = [token for token in required_tokens if token not in text]
report = {
    "status": "passed" if not missing else "failed",
    "source_path": str(source_path),
    "required_tokens": required_tokens,
    "missing_tokens": missing,
    "error_code": None if not missing else "missing_runtime_compat_token",
}
report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
print(json.dumps(report))
sys.exit(0 if not missing else 1)
PY
}

validate_workflow_surface() {
  local source_path="$1"
  local report_path="$2"
  python3 - "$source_path" "$report_path" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
text = source_path.read_text(encoding="utf-8")

required_tokens = [
    "robot state",
    "robot get-text",
    "robot send",
    "robot wait-for",
    "robot search",
    "robot events",
    "snapshot",
    "session",
]

missing = [token for token in required_tokens if token not in text]
report = {
    "status": "passed" if not missing else "failed",
    "source_path": str(source_path),
    "required_tokens": required_tokens,
    "missing_tokens": missing,
    "error_code": None if not missing else "missing_workflow_surface_token",
}
report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
print(json.dumps(report))
sys.exit(0 if not missing else 1)
PY
}

cd "${ROOT_DIR}"
: > "${STDOUT_FILE}"

require_cmd jq
require_cmd rch
require_cmd cargo
require_cmd python3

emit_log "preflight" "startup" "scenario_start" "started" "none" "none" "$(basename "${LOG_FILE}")"

probe_log="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}_rch_probe.json"
set +e
rch workers probe --all --json > "${probe_log}" 2>>"${STDOUT_FILE}"
probe_rc=$?
set -e

if [[ ${probe_rc} -ne 0 ]]; then
  emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_probe_failed" "RCH-E100" "$(basename "${probe_log}")"
  echo "rch workers probe failed" >&2
  exit 2
fi

healthy_workers=$(jq '[.data[]? | select(.status == "ok" or .status == "healthy" or .status == "reachable")] | length' "${probe_log}")
if [[ "${healthy_workers}" -lt 1 ]]; then
  emit_log "preflight" "rch_probe" "workers_probe" "failed" "rch_workers_unreachable" "RCH-E100" "$(basename "${probe_log}")"
  echo "no reachable rch workers; refusing local fallback" >&2
  exit 2
fi

emit_log "preflight" "rch_probe" "workers_probe" "passed" "workers_reachable" "none" "$(basename "${probe_log}")"

IPC_SOURCE="${ROOT_DIR}/crates/frankenterm-core/src/ipc.rs"
HARNESS_SOURCE="${ROOT_DIR}/scripts/e2e_test.sh"

emit_log "validation" "contract_path" "ipc_runtime_compat_contract" "running" "none" "none" "$(basename "${VALIDATOR_PASS_REPORT}")"
if validate_ipc_contract "${IPC_SOURCE}" "${VALIDATOR_PASS_REPORT}"; then
  emit_log "validation" "contract_path" "ipc_runtime_compat_contract" "passed" "contract_tokens_present" "none" "$(basename "${VALIDATOR_PASS_REPORT}")"
else
  emit_log "validation" "contract_path" "ipc_runtime_compat_contract" "failed" "missing_contract_tokens" "IPC-CONTRACT-FAIL" "$(basename "${VALIDATOR_PASS_REPORT}")"
  exit 1
fi

emit_log "validation" "workflow_surface_path" "robot_watch_session_snapshot_surface" "running" "none" "none" "$(basename "${WORKFLOW_SURFACE_REPORT}")"
if validate_workflow_surface "${HARNESS_SOURCE}" "${WORKFLOW_SURFACE_REPORT}"; then
  emit_log "validation" "workflow_surface_path" "robot_watch_session_snapshot_surface" "passed" "workflow_surface_tokens_present" "none" "$(basename "${WORKFLOW_SURFACE_REPORT}")"
else
  emit_log "validation" "workflow_surface_path" "robot_watch_session_snapshot_surface" "failed" "workflow_surface_missing" "WORKFLOW-SURFACE-FAIL" "$(basename "${WORKFLOW_SURFACE_REPORT}")"
  exit 1
fi

emit_log "validation" "nominal_path" "asupersync_unix_socket_tests" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step nominal_socket_tests \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test asupersync_unix_socket -- --nocapture; then
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "asupersync_unix_socket_tests" "passed" "socket_tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "asupersync_unix_socket_tests" "failed" "socket_tests_failed" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

emit_log "validation" "nominal_path" "ipc_restart_and_degraded_tests" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step nominal_ipc_tests \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --lib ipc::tests::ipc_server_restarts_cleanly_on_same_socket_path -- --nocapture; then
  ensure_no_local_fallback
else
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "ipc_restart_and_degraded_tests" "failed" "ipc_restart_test_failed" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

if run_step nominal_ipc_degraded \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --lib ipc::tests::ipc_client_reports_server_closed_without_response -- --nocapture; then
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "ipc_restart_and_degraded_tests" "passed" "ipc_targeted_tests_passed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "ipc_restart_and_degraded_tests" "failed" "ipc_degraded_test_failed" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

tmp_mutated="$(mktemp "${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.mutated.XXXXXX.rs")"
cleanup() {
  rm -f "${tmp_mutated}"
}
trap cleanup EXIT

python3 - "${IPC_SOURCE}" "${tmp_mutated}" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text(encoding="utf-8")
mutated = source.replace("compat_unix::connect(", "legacy_unix_connect(", 1)
Path(sys.argv[2]).write_text(mutated, encoding="utf-8")
PY

emit_log "validation" "failure_injection_path" "mutated_ipc_contract" "running" "none" "none" "$(basename "${tmp_mutated}")"
set +e
validate_ipc_contract "${tmp_mutated}" "${VALIDATOR_FAIL_REPORT}" >> "${STDOUT_FILE}" 2>&1
fail_rc=$?
set -e
if [[ ${fail_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection_path" "mutated_ipc_contract" "failed" "failure_injection_not_detected" "EXPECTED-FAILURE-NOT-TRIGGERED" "$(basename "${VALIDATOR_FAIL_REPORT}")"
  exit 1
fi
if ! jq -e '.status == "failed" and .error_code == "missing_runtime_compat_token"' "${VALIDATOR_FAIL_REPORT}" >/dev/null; then
  emit_log "validation" "failure_injection_path" "mutated_ipc_contract" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${VALIDATOR_FAIL_REPORT}")"
  exit 1
fi
emit_log "validation" "failure_injection_path" "mutated_ipc_contract" "passed" "expected_failure_detected" "none" "$(basename "${VALIDATOR_FAIL_REPORT}")"

emit_log "validation" "recovery_path" "original_ipc_contract_recheck" "running" "none" "none" "$(basename "${VALIDATOR_RECOVERY_REPORT}")"
if validate_ipc_contract "${IPC_SOURCE}" "${VALIDATOR_RECOVERY_REPORT}"; then
  emit_log "validation" "recovery_path" "original_ipc_contract_recheck" "passed" "recovery_validated" "none" "$(basename "${VALIDATOR_RECOVERY_REPORT}")"
else
  emit_log "validation" "recovery_path" "original_ipc_contract_recheck" "failed" "recovery_validation_failed" "RECOVERY-FAILED" "$(basename "${VALIDATOR_RECOVERY_REPORT}")"
  exit 1
fi

emit_log "summary" "contract->nominal->failure_injection->recovery" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
echo "ft-e34d9.10.4.1 unix IPC e2e scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
