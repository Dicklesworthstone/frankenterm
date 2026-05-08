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
LOG_FILE="${LOG_DIR}/test_agent_autoconfig_$(date +%Y%m%dT%H%M%S).jsonl"
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

# ---- Test: Autoconfig integration tests via cargo ----
echo "=== Running autoconfig integration tests ==="
log_json "autoconfig_integration" "detect" "running" "Starting cargo test for autoconfig"

if rch exec -- cargo test -p frankenterm-core integration_agent_autoconfig --no-default-features -- --nocapture 2>&1; then
    log_json "autoconfig_integration" "assert" "pass" "All autoconfig integration tests passed"
    PASS=$((PASS + 1))
else
    log_json "autoconfig_integration" "assert" "fail" "Autoconfig integration tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test: Inline agent_config_templates tests ----
echo "=== Running inline config template tests ==="
log_json "config_templates_inline" "detect" "running" "Starting inline tests"

if rch exec -- cargo test -p frankenterm-core agent_config_templates --no-default-features -- --nocapture 2>&1; then
    log_json "config_templates_inline" "assert" "pass" "All inline config template tests passed"
    PASS=$((PASS + 1))
else
    log_json "config_templates_inline" "assert" "fail" "Inline config template tests failed"
    FAIL=$((FAIL + 1))
fi

# ---- Test: Proptest agent config templates ----
echo "=== Running proptest config template tests ==="
log_json "config_templates_proptest" "detect" "running" "Starting proptest suite"

if rch exec -- cargo test -p frankenterm-core proptest_agent_config_templates --no-default-features -- --nocapture 2>&1; then
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
