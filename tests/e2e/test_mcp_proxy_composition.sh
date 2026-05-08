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
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-mcp-proxy-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\r'/\\r}
  value=${value//$'\t'/\\t}
  printf '%s' "$value"
}

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
    "$(json_escape "${now}")" \
    "$(json_escape "${COMPONENT}")" \
    "$(json_escape "${SCENARIO_ID}")" \
    "$(json_escape "${CORRELATION_ID}")" \
    "$(json_escape "${decision_path}")" \
    "$(json_escape "${input_summary}")" \
    "$(json_escape "${outcome}")" \
    "$(json_escape "${reason_code}")" \
    "$(json_escape "${error_code}")" \
    "$(json_escape "${artifact_path}")" \
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

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "mcp_proxy_composition"
ensure_rch_ready

log_event "passed" "rch_workers_available" "none" "preflight>workers_probe" \
  "shared rch guard reported reachable workers" "$(rch_probe_log_path)"
log_event "passed" "rch_remote_smoke_passed" "none" "preflight>remote_smoke" \
  "shared rch guard verified fail-closed remote cargo execution" "$(rch_smoke_log_path)"

CARGO_CMD=(
  cargo test -p frankenterm-core --features "mcp,mcp-client" --test mcp_proxy_integration -- --nocapture
)

log_event "running" "invoke_rch_cargo_test" "none" "execute>cargo_test" \
  "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${CARGO_CMD[*]}" "${STDOUT_LOG}"
set +e
run_rch_cargo_logged "${STDOUT_LOG}" env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" "${CARGO_CMD[@]}"
status=$?
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
