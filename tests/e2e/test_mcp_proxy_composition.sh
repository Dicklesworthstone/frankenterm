#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
SCENARIO_ID="ft-1fv0u.7"
COMPONENT="tests.e2e.mcp_proxy_composition"
CORRELATION_ID="mcp_proxy_composition_${RUN_ID}"
LOG_FILE="${LOG_DIR}/mcp_proxy_composition_${RUN_ID}.jsonl"
STDOUT_LOG="${LOG_DIR}/mcp_proxy_composition_${RUN_ID}.stdout.log"
RCH_PROBE_LOG="${LOG_DIR}/mcp_proxy_composition_${RUN_ID}.rch_probe.json"

log_event() {
  local outcome="$1"
  local reason_code="$2"
  local error_code="$3"
  local decision_path="$4"
  local input_summary="$5"
  local artifact_path="$6"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"timestamp":"%s","component":"%s","scenario_id":"%s","correlation_id":"%s","decision_path":"%s","input_summary":"%s","outcome":"%s","reason_code":"%s","error_code":"%s","artifact_path":"%s"}\n' \
    "${now}" "${COMPONENT}" "${SCENARIO_ID}" "${CORRELATION_ID}" "${decision_path}" "${input_summary}" "${outcome}" "${reason_code}" "${error_code}" "${artifact_path}" \
    | tee -a "${LOG_FILE}"
}

log_event "start" "begin" "none" "setup>start" "starting MCP proxy composition e2e validation" "${LOG_FILE}"
log_event "context" "paths_ready" "none" "setup>context" "root=${ROOT_DIR}" "${STDOUT_LOG}"

if ! command -v rch >/dev/null 2>&1; then
  log_event "failed" "missing_rch" "RCH_MISSING" "preflight>check_rch" "rch is required for offloaded cargo execution" "${LOG_FILE}"
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  log_event "degraded" "missing_python3" "PYTHON_MISSING" "preflight>check_python3" "python3 not found; proxy integration tests may be skipped by harness" "${LOG_FILE}"
fi

if ! command -v jq >/dev/null 2>&1; then
  log_event "failed" "missing_jq" "JQ_MISSING" "preflight>check_jq" "jq is required to validate rch worker readiness" "${LOG_FILE}"
  exit 1
fi

if ! rch workers probe --all --json >"${RCH_PROBE_LOG}"; then
  log_event "failed" "rch_probe_failed" "RCH_PROBE_FAILED" "preflight>workers_probe" "rch workers probe command failed" "${RCH_PROBE_LOG}"
  exit 1
fi
# rch probe has returned both "healthy" and "ok" across versions; accept either
healthy_workers="$(jq '[.data[] | select(((.status // "") | ascii_downcase) == "healthy" or ((.status // "") | ascii_downcase) == "ok")] | length' "${RCH_PROBE_LOG}")"
if [[ "${healthy_workers}" -lt 1 ]]; then
  log_event "failed" "rch_workers_unavailable" "RCH_WORKERS_DOWN" "preflight>workers_probe" "no healthy rch workers; refusing local fallback" "${RCH_PROBE_LOG}"
  exit 1
fi
log_event "passed" "rch_workers_available" "none" "preflight>workers_probe" "healthy_rch_workers=${healthy_workers}" "${RCH_PROBE_LOG}"

TEST_CMD=(
  rch exec --
  env CARGO_TARGET_DIR=target-rch-mcp-proxy
  cargo test -p frankenterm-core --features mcp,mcp-client --test mcp_proxy_integration -- --nocapture
)

log_event "running" "invoke_rch_cargo_test" "none" "execute>cargo_test" "${TEST_CMD[*]}" "${STDOUT_LOG}"
set +e
(
  cd "${ROOT_DIR}"
  "${TEST_CMD[@]}"
) 2>&1 | tee -a "${STDOUT_LOG}"
status=${PIPESTATUS[0]}
set -e

if [[ ${status} -ne 0 ]]; then
  log_event "failed" "cargo_test_failed" "CARGO_TEST_FAILED" "assert>cargo_test_exit" "MCP proxy integration tests failed with exit=${status}" "${STDOUT_LOG}"
  exit "${status}"
fi

if grep -q "\\[RCH\\] local (remote execution failed)" "${STDOUT_LOG}"; then
  log_event "failed" "rch_local_fallback_detected" "RCH_FAIL_OPEN_LOCAL" "assert>offload_only" "rch fail-opened to local execution; refusing offload policy violation" "${STDOUT_LOG}"
  exit 1
fi

if grep -q "remote/mock/echo" "${STDOUT_LOG}"; then
  log_event "passed" "route_prefix_observed" "none" "assert>route_marker" "observed prefixed route marker remote/mock/echo in test output" "${STDOUT_LOG}"
else
  log_event "failed" "missing_route_marker" "ROUTE_MARKER_MISSING" "assert>route_marker" "did not observe expected proxied route marker remote/mock/echo" "${STDOUT_LOG}"
  exit 1
fi

log_event "passed" "e2e_complete" "none" "complete" "MCP proxy composition e2e validation completed successfully" "${LOG_FILE}"
