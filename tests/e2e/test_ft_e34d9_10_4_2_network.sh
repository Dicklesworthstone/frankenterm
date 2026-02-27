#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"

RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft_e34d9_10_4_2_network"
CORRELATION_ID="ft-e34d9.10.4.2-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
STDOUT_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.stdout.log"
RUNTIME_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.runtime_contract.json"
WEB_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.web_contract.json"
HARNESS_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.harness_contract.json"
FAILURE_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.failure_injection.json"
RECOVERY_REPORT="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.recovery_contract.json"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rch-e2e-ft-e34d9-10-4-2-network}"
export CARGO_TARGET_DIR

LAST_STEP_LOG=""

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

validate_tokens() {
  local source_path="$1"
  local report_path="$2"
  local error_code="$3"
  shift 3
  python3 - "$source_path" "$report_path" "$error_code" "$@" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
error_code = sys.argv[3]
required_tokens = sys.argv[4:]

text = source_path.read_text(encoding="utf-8")
missing = [token for token in required_tokens if token not in text]
report = {
    "status": "passed" if not missing else "failed",
    "source_path": str(source_path),
    "required_tokens": required_tokens,
    "missing_tokens": missing,
    "error_code": None if not missing else error_code,
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

RUNTIME_SOURCE="${ROOT_DIR}/crates/frankenterm-core/src/runtime_compat.rs"
WEB_TEST_SOURCE="${ROOT_DIR}/crates/frankenterm-core/tests/web.rs"
HARNESS_SOURCE="${ROOT_DIR}/scripts/e2e_test.sh"

emit_log "validation" "contract_path" "runtime_compat_network_tokens" "running" "none" "none" "$(basename "${RUNTIME_REPORT}")"
if validate_tokens "${RUNTIME_SOURCE}" "${RUNTIME_REPORT}" "missing_runtime_compat_network_token" \
  "pub mod io {" \
  "pub use asupersync::io::{AsyncReadExt, AsyncWriteExt};" \
  "pub use tokio::io::{AsyncReadExt, AsyncWriteExt};" \
  "pub mod net {" \
  "pub use asupersync::net::{TcpListener, TcpStream};" \
  "pub use tokio::net::{TcpListener, TcpStream};" \
  "pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, String>"; then
  emit_log "validation" "contract_path" "runtime_compat_network_tokens" "passed" "runtime_compat_contract_present" "none" "$(basename "${RUNTIME_REPORT}")"
else
  emit_log "validation" "contract_path" "runtime_compat_network_tokens" "failed" "runtime_compat_contract_missing" "RUNTIME-COMPAT-CONTRACT-FAIL" "$(basename "${RUNTIME_REPORT}")"
  exit 1
fi

emit_log "validation" "contract_path" "web_network_timeout_contract" "running" "none" "none" "$(basename "${WEB_REPORT}")"
if validate_tokens "${WEB_TEST_SOURCE}" "${WEB_REPORT}" "missing_web_network_timeout_token" \
  "stream_fetch_prefix_times_out_on_stalled_body" \
  "io::{AsyncReadExt, AsyncWriteExt}" \
  "net::{TcpListener, TcpStream}" \
  "fetch_stream_prefix(addr, req, Duration::from_millis(120), 256).await?" \
  "response.contains(\"HTTP/1.1 200 OK\")"; then
  emit_log "validation" "contract_path" "web_network_timeout_contract" "passed" "web_timeout_contract_present" "none" "$(basename "${WEB_REPORT}")"
else
  emit_log "validation" "contract_path" "web_network_timeout_contract" "failed" "web_timeout_contract_missing" "WEB-CONTRACT-FAIL" "$(basename "${WEB_REPORT}")"
  exit 1
fi

emit_log "validation" "workflow_surface_path" "harness_scenario_registration" "running" "none" "none" "$(basename "${HARNESS_REPORT}")"
if validate_tokens "${HARNESS_SOURCE}" "${HARNESS_REPORT}" "missing_harness_scenario_token" \
  "ft_e34d9_10_4_2_network" \
  "run_scenario_ft_e34d9_10_4_2_network"; then
  emit_log "validation" "workflow_surface_path" "harness_scenario_registration" "passed" "harness_registration_present" "none" "$(basename "${HARNESS_REPORT}")"
else
  emit_log "validation" "workflow_surface_path" "harness_scenario_registration" "failed" "harness_registration_missing" "HARNESS-CONTRACT-FAIL" "$(basename "${HARNESS_REPORT}")"
  exit 1
fi

emit_log "validation" "nominal_path" "web_timeout_test_remote" "running" "none" "none" "$(basename "${STDOUT_FILE}")"
if run_step web_timeout_test \
  rch exec -- env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test -p frankenterm-core --test web --features web stream_fetch_prefix_times_out_on_stalled_body -- --nocapture; then
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "web_timeout_test_remote" "passed" "web_timeout_test_passed" "none" "$(basename "${LAST_STEP_LOG}")"
else
  ensure_no_local_fallback
  emit_log "validation" "nominal_path" "web_timeout_test_remote" "failed" "web_timeout_test_failed" "CARGO-TEST-FAIL" "$(basename "${LAST_STEP_LOG}")"
  exit 1
fi

tmp_mutated="$(mktemp "${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.mutated.XXXXXX.rs")"
cleanup() {
  rm -f "${tmp_mutated}"
}
trap cleanup EXIT

python3 - "${WEB_TEST_SOURCE}" "${tmp_mutated}" <<'PY'
import sys
from pathlib import Path

source = Path(sys.argv[1]).read_text(encoding="utf-8")
mutated = source.replace("stream_fetch_prefix_times_out_on_stalled_body", "stream_fetch_prefix_timeout_removed", 1)
Path(sys.argv[2]).write_text(mutated, encoding="utf-8")
PY

emit_log "validation" "failure_injection_path" "mutated_web_timeout_contract" "running" "none" "none" "$(basename "${FAILURE_REPORT}")"
set +e
validate_tokens "${tmp_mutated}" "${FAILURE_REPORT}" "missing_web_network_timeout_token" \
  "stream_fetch_prefix_times_out_on_stalled_body" \
  "io::{AsyncReadExt, AsyncWriteExt}" \
  "net::{TcpListener, TcpStream}" \
  "fetch_stream_prefix(addr, req, Duration::from_millis(120), 256).await?" \
  "response.contains(\"HTTP/1.1 200 OK\")" >> "${STDOUT_FILE}" 2>&1
fail_rc=$?
set -e
if [[ ${fail_rc} -eq 0 ]]; then
  emit_log "validation" "failure_injection_path" "mutated_web_timeout_contract" "failed" "failure_injection_not_detected" "EXPECTED-FAILURE-NOT-TRIGGERED" "$(basename "${FAILURE_REPORT}")"
  exit 1
fi
if ! jq -e '.status == "failed" and .error_code == "missing_web_network_timeout_token"' "${FAILURE_REPORT}" >/dev/null; then
  emit_log "validation" "failure_injection_path" "mutated_web_timeout_contract" "failed" "unexpected_failure_signature" "FAILURE-SIGNATURE-MISSING" "$(basename "${FAILURE_REPORT}")"
  exit 1
fi
emit_log "validation" "failure_injection_path" "mutated_web_timeout_contract" "passed" "expected_failure_detected" "none" "$(basename "${FAILURE_REPORT}")"

emit_log "validation" "recovery_path" "web_timeout_contract_recheck" "running" "none" "none" "$(basename "${RECOVERY_REPORT}")"
if validate_tokens "${WEB_TEST_SOURCE}" "${RECOVERY_REPORT}" "missing_web_network_timeout_token" \
  "stream_fetch_prefix_times_out_on_stalled_body" \
  "io::{AsyncReadExt, AsyncWriteExt}" \
  "net::{TcpListener, TcpStream}" \
  "fetch_stream_prefix(addr, req, Duration::from_millis(120), 256).await?" \
  "response.contains(\"HTTP/1.1 200 OK\")"; then
  emit_log "validation" "recovery_path" "web_timeout_contract_recheck" "passed" "recovery_validated" "none" "$(basename "${RECOVERY_REPORT}")"
else
  emit_log "validation" "recovery_path" "web_timeout_contract_recheck" "failed" "recovery_validation_failed" "RECOVERY-FAILED" "$(basename "${RECOVERY_REPORT}")"
  exit 1
fi

emit_log "summary" "contract->nominal->failure_injection->recovery" "scenario_complete" "passed" "all_checks_passed" "none" "$(basename "${STDOUT_FILE}")"
echo "ft-e34d9.10.4.2 network e2e scenario passed. Logs: ${LOG_FILE#"${ROOT_DIR}/"}"
