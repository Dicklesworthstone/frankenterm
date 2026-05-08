#!/usr/bin/env bash
# test_agent_autoconfig.sh — E2E: Agent autoconfig generation and idempotency (ft-dr6zv.2.5)
#
# Validates:
# - Config template generation produces valid content for all known agents
# - Merge is idempotent (run twice → same result)
# - No stale commands in generated templates
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="${SCRIPT_DIR}/logs"
RUN_ID="$(date +%Y%m%dT%H%M%S)"
LOG_FILE="${LOG_DIR}/test_agent_autoconfig_${RUN_ID}.jsonl"
DEFAULT_CARGO_TARGET_DIR="target/rch-e2e-agent-autoconfig-${RUN_ID}"
INHERITED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
if [[ -n "${INHERITED_CARGO_TARGET_DIR}" && "${INHERITED_CARGO_TARGET_DIR}" != /* ]]; then
    CARGO_TARGET_DIR="${INHERITED_CARGO_TARGET_DIR}"
else
    CARGO_TARGET_DIR="${DEFAULT_CARGO_TARGET_DIR}"
fi
export CARGO_TARGET_DIR
PASS=0
FAIL=0

mkdir -p "$LOG_DIR"

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
    local test_name="$1" phase="$2" result="$3" detail="$4"
    local ts_ms
    ts_ms=$(python3 -c "import time; print(int(time.time()*1000))" 2>/dev/null || date +%s000)
    printf '{"test_name":"%s","phase":"%s","timestamp_ms":%s,"result":"%s","detail":"%s"}\n' \
        "$(json_escape "$test_name")" \
        "$(json_escape "$phase")" \
        "$ts_ms" \
        "$(json_escape "$result")" \
        "$(json_escape "$detail")" >> "$LOG_FILE"
}

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for shared rch metadata" >&2
    exit 1
fi

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "${SCRIPT_DIR}/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "agent_autoconfig"
ensure_rch_ready

run_cargo_test() {
    local step="$1"
    shift
    local output_file="${LOG_DIR}/test_agent_autoconfig_${RUN_ID}.${step}.stdout.log"
    local status

    if run_rch_cargo_logged "${output_file}" \
        env CARGO_TARGET_DIR="${CARGO_TARGET_DIR}" cargo test "$@"; then
        status=0
    else
        status=$?
    fi

    cat "${output_file}"
    return "${status}"
}

# ---- Test: Autoconfig integration tests via cargo ----
echo "=== Running autoconfig integration tests ==="
log_json "autoconfig_integration" "detect" "running" "Starting cargo test for autoconfig"

if run_cargo_test autoconfig_integration \
    -p frankenterm-core integration_agent_autoconfig --no-default-features -- --nocapture; then
    log_json "autoconfig_integration" "assert" "pass" "All autoconfig integration tests passed"
    PASS=$((PASS + 1))
else
    log_json "autoconfig_integration" "assert" "fail" "Autoconfig integration tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test: Inline agent_config_templates tests ----
echo "=== Running inline config template tests ==="
log_json "config_templates_inline" "detect" "running" "Starting inline tests"

if run_cargo_test config_templates_inline \
    -p frankenterm-core agent_config_templates --no-default-features -- --nocapture; then
    log_json "config_templates_inline" "assert" "pass" "All inline config template tests passed"
    PASS=$((PASS + 1))
else
    log_json "config_templates_inline" "assert" "fail" "Inline config template tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test: Proptest agent config templates ----
echo "=== Running proptest config template tests ==="
log_json "config_templates_proptest" "detect" "running" "Starting proptest suite"

if run_cargo_test config_templates_proptest \
    -p frankenterm-core proptest_agent_config_templates --no-default-features -- --nocapture; then
    log_json "config_templates_proptest" "assert" "pass" "All proptest config template tests passed"
    PASS=$((PASS + 1))
else
    log_json "config_templates_proptest" "assert" "fail" "Proptest config template tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Summary ----
echo ""
echo "=== Agent Autoconfig E2E Summary ==="
echo "  Pass: $PASS"
echo "  Fail: $FAIL"
echo "  Log:  $LOG_FILE"

log_json "summary" "teardown" "$([ $FAIL -eq 0 ] && echo pass || echo fail)" "Pass=$PASS Fail=$FAIL"

[ "$FAIL" -eq 0 ]
