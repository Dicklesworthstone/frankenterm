#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
mkdir -p "${LOG_DIR}"
RUN_ID="$(date +"%Y%m%d_%H%M%S")"
LOG_FILE="${LOG_DIR}/agent_provider_bridge_integration_${RUN_ID}.log"
RCH_LOG="${LOG_DIR}/agent_provider_bridge_integration_${RUN_ID}.rch.log"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-agent-provider-bridge-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
  CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
  CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"

json_escape() {
  local value="$1"
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//$'\n'/\\n}
  value=${value//$'\r'/\\r}
  value=${value//$'\t'/\\t}
  printf '%s' "$value"
}

log_json() {
  local level="$1"
  local event="$2"
  local message="$3"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"ts":"%s","level":"%s","event":"%s","message":"%s"}\n' \
    "$(json_escape "${now}")" \
    "$(json_escape "${level}")" \
    "$(json_escape "${event}")" \
    "$(json_escape "${message}")" | tee -a "${LOG_FILE}"
}

log_json "info" "start" "Starting agent provider bridge integration e2e"
log_json "info" "context" "root=${ROOT_DIR} log=${LOG_FILE}"

if ! command -v jq >/dev/null 2>&1; then
  log_json "error" "missing_jq" "jq is required for rch metadata artifacts"
  exit 1
fi

rch_init "${LOG_DIR}" "${RUN_ID}" "agent_provider_bridge_integration"
ensure_rch_ready

log_json "info" "run_tests" "Executing via rch: cargo test -p frankenterm-core --test agent_provider_bridge_integration -- --nocapture"
set +e
run_rch_cargo_logged "${RCH_LOG}" \
  env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" \
  cargo test -p frankenterm-core --test agent_provider_bridge_integration -- --nocapture
status=$?
set -e
tee -a "${LOG_FILE}" < "${RCH_LOG}"

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
