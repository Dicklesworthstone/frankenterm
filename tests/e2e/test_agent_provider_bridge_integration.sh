#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
LOG_FILE="${LOG_DIR}/agent_provider_bridge_integration_${RUN_ID}.log"

log_json() {
  local level="$1"
  local event="$2"
  local message="$3"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"ts":"%s","level":"%s","event":"%s","message":"%s"}\n' \
    "${now}" "${level}" "${event}" "${message}" | tee -a "${LOG_FILE}"
}

log_json "info" "start" "Starting agent provider bridge integration e2e"
log_json "info" "context" "root=${ROOT_DIR} log=${LOG_FILE}"

if ! command -v rch >/dev/null 2>&1; then
  log_json "error" "missing_rch" "rch is required for offloaded cargo execution"
  exit 1
fi

TEST_CMD=(
  rch exec --
  env CARGO_TARGET_DIR=target-rch-agent-provider-bridge
  cargo test -p frankenterm-core --test agent_provider_bridge_integration -- --nocapture
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
  log_json "error" "test_failure" "command failed with exit=${status}"
  exit "${status}"
fi

if grep -q "test result: ok" "${LOG_FILE}"; then
  log_json "info" "result_check" "Detected passing cargo test summary"
else
  log_json "error" "result_check_failed" "Did not find passing test summary in log output"
  exit 1
fi

log_json "info" "success" "Agent provider bridge integration e2e completed successfully"
