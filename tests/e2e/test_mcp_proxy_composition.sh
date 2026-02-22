#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
LOG_FILE="${LOG_DIR}/mcp_proxy_composition_${RUN_ID}.log"

log_json() {
  local level="$1"
  local event="$2"
  local message="$3"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"ts":"%s","level":"%s","event":"%s","message":"%s"}\n' \
    "${now}" "${level}" "${event}" "${message}" | tee -a "${LOG_FILE}"
}

log_json "info" "start" "Starting MCP proxy composition e2e validation"
log_json "info" "context" "root=${ROOT_DIR} log=${LOG_FILE}"

if ! command -v rch >/dev/null 2>&1; then
  log_json "error" "missing_rch" "rch is required for offloaded cargo execution"
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  log_json "warn" "missing_python3" "python3 not found; proxy integration tests will be skipped by test harness"
fi

TEST_CMD=(
  rch exec --
  env CARGO_TARGET_DIR=target-rch-mcp-proxy
  cargo test -p frankenterm-core --features mcp,mcp-client --test mcp_proxy_integration -- --nocapture
)

log_json "info" "run_tests" "Executing: ${TEST_CMD[*]}"
set +e
(
  cd "${ROOT_DIR}"
  "${TEST_CMD[@]}"
) 2>&1 | tee -a "${LOG_FILE}"
status=${PIPESTATUS[0]}
set -e

if [[ ${status} -ne 0 ]]; then
  log_json "error" "test_failure" "MCP proxy integration test command failed with exit=${status}"
  exit "${status}"
fi

if grep -q "remote/mock/echo" "${LOG_FILE}"; then
  log_json "info" "route_assertion" "Observed prefixed proxied tool route in test output"
else
  log_json "error" "route_assertion_failed" "Did not observe expected proxied route marker (remote/mock/echo)"
  exit 1
fi

log_json "info" "success" "MCP proxy composition e2e validation completed successfully"
