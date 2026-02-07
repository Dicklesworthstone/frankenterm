#!/bin/bash
# =============================================================================
# E2E: Reliability hardening scenarios with verbose logs
# Implements: bd-1zgb
#
# Purpose:
#   Validate resilience features end-to-end:
#   - Circuit breaker: open/half-open/closed state transitions
#   - Retry logic: exponential backoff, jitter, attempt exhaustion
#   - Graceful degradation: subsystem isolation, write queuing, recovery
#   - Fault injection: chaos scenarios, assertions, cascading failures
#   - Health monitoring: watchdog, heartbeats, staleness detection
#   - Integration: retry+circuit, degradation+chaos
#
# Requirements:
#   - cargo (Rust toolchain)
#   - jq for JSON manipulation
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

# Colors (disabled when piped)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0

# Configuration
VERBOSE=false

# ==============================================================================
# Argument parsing
# ==============================================================================

while [[ $# -gt 0 ]]; do
    case "$1" in
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--verbose]" >&2
            exit 3
            ;;
    esac
done

# ==============================================================================
# Logging
# ==============================================================================

log_test() {
    echo -e "\n${BLUE}=== $1 ===${NC}"
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $*"
    ((TESTS_PASSED++)) || true
    ((TESTS_RUN++)) || true
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $*"
    ((TESTS_FAILED++)) || true
    ((TESTS_RUN++)) || true
}

log_skip() {
    echo -e "${YELLOW}[SKIP]${NC} $*"
    ((TESTS_SKIPPED++)) || true
}

log_info() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo -e "       $*"
    fi
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    if ! command -v cargo &>/dev/null; then
        echo -e "${RED}ERROR:${NC} cargo not found. Install Rust toolchain." >&2
        exit 5
    fi
    log_pass "cargo available"

    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found. Install: sudo apt install jq" >&2
        exit 5
    fi
    log_pass "jq available"
}

# ==============================================================================
# Scenario 1: Circuit breaker state transitions
# ==============================================================================

scenario_circuit_breaker() {
    log_test "Scenario 1: Circuit breaker state transitions"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'circuit_breaker::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "circuit_breaker.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: Circuit breaker tests pass"
    else
        log_fail "S1.1: Circuit breaker tests failed (exit=$exit_code)"
    fi

    for test_name in circuit_opens_after_threshold circuit_half_open_closes_on_success circuit_half_open_failure_reopens; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S1.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S1.${TESTS_RUN}: $test_name missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: Retry logic with exponential backoff
# ==============================================================================

scenario_retry_logic() {
    log_test "Scenario 2: Retry logic with exponential backoff"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'retry::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "retry_logic.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: Retry logic tests pass"
    else
        log_fail "S2.1: Retry logic tests failed (exit=$exit_code)"
    fi

    for test_name in delay_calculation_with_backoff jitter_within_range retry_exhausts_attempts circuit_breaker_integration preset_policies_have_sensible_defaults; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S2.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S2.${TESTS_RUN}: $test_name missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: Graceful degradation and recovery
# ==============================================================================

scenario_graceful_degradation() {
    log_test "Scenario 3: Graceful degradation and recovery"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'degradation::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "graceful_degradation.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: Graceful degradation tests pass"
    else
        log_fail "S3.1: Graceful degradation tests failed (exit=$exit_code)"
    fi

    for test_name in enter_degraded_mode enter_unavailable_mode recover_returns_to_normal queued_writes_bounded paused_workflows multiple_degradations; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S3.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S3.${TESTS_RUN}: $test_name missing"
        fi
    done

    # Count degradation tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 20 ]]; then
        log_pass "S3.${TESTS_RUN}: $passed_count degradation tests validated (>= 20)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S3.${TESTS_RUN}: Only $passed_count degradation tests (expected >= 20)"
    else
        log_fail "S3.${TESTS_RUN}: Could not parse degradation test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Fault injection and chaos scenarios
# ==============================================================================

scenario_fault_injection() {
    log_test "Scenario 4: Fault injection and chaos scenarios"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'chaos::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fault_injection.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Chaos / fault injection tests pass"
    else
        log_fail "S4.1: Chaos tests failed (exit=$exit_code)"
    fi

    # Pre-built chaos scenarios
    for test_name in scenario_db_write_failure scenario_wezterm_unavailable scenario_cascading_failures scenario_pattern_engine_failure; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S4.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S4.${TESTS_RUN}: $test_name missing"
        fi
    done

    # Chaos assertion system
    for test_name in assertion_fault_never_fired_passes_when_clean assertion_fault_never_fired_fails_when_fired total_faults_in_range_assertion; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S4.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S4.${TESTS_RUN}: $test_name missing"
        fi
    done

    # Count chaos tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 15 ]]; then
        log_pass "S4.${TESTS_RUN}: $passed_count chaos tests validated (>= 15)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S4.${TESTS_RUN}: Only $passed_count chaos tests (expected >= 15)"
    else
        log_fail "S4.${TESTS_RUN}: Could not parse chaos test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Health monitoring (watchdog + heartbeats)
# ==============================================================================

scenario_health_monitoring() {
    log_test "Scenario 5: Health monitoring (watchdog + heartbeats)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'watchdog::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "health_monitoring.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Watchdog tests pass"
    else
        log_fail "S5.1: Watchdog tests failed (exit=$exit_code)"
    fi

    for test_name in active_heartbeats_are_healthy stale_heartbeat_is_degraded very_stale_heartbeat_is_critical health_report_serializes watchdog_shuts_down_on_signal; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S5.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S5.${TESTS_RUN}: $test_name missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Cross-module integration (retry+circuit, degradation+chaos)
# ==============================================================================

scenario_cross_module_integration() {
    log_test "Scenario 6: Cross-module integration"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run retry+circuit integration and global injector lifecycle
    cargo test -p wa-core 'circuit_breaker_integration\|global_injector_lifecycle\|free_functions_fail_open' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "cross_module_integration.log" "$(cat "$test_output")"

    # Since cargo test doesn't support \| alternation, run each separately
    # First check if any ran (might be 0 due to filter issue)
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -eq 0 ]]; then
        # Cargo test doesn't support \| — run a broader filter
        exit_code=0
        cargo test -p wa-core 'circuit_breaker_integration' \
            --no-fail-fast -- --nocapture \
            >"$test_output" 2>&1 || exit_code=$?
    fi

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Retry + circuit breaker integration passes"
    else
        log_fail "S6.1: Integration test failed (exit=$exit_code)"
    fi

    # Verify the integration test ran
    if command grep -q "circuit_breaker_integration" "$test_output"; then
        log_pass "S6.2: Retry exhaustion triggers circuit open"
    else
        log_fail "S6.2: circuit_breaker_integration test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Bounded execution (full reliability suite timing)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 7: Bounded execution (reliability suite timing)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    # Run all 5 reliability modules together
    timeout 120 cargo test -p wa-core 'circuit_breaker::tests\|retry::tests\|degradation::tests\|chaos::tests\|watchdog::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    # If the \| filter didn't work, run a broader match
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -eq 0 ]]; then
        # Fallback: run each module and sum
        local total_passed=0 total_failed=0
        for module in circuit_breaker::tests retry::tests degradation::tests chaos::tests watchdog::tests; do
            local mod_output mod_exit
            mod_output=$(mktemp)
            mod_exit=0
            timeout 30 cargo test -p wa-core "$module" \
                --no-fail-fast -- --nocapture \
                >"$mod_output" 2>&1 || mod_exit=$?

            local mp=0
            while IFS= read -r line; do
                local n
                n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
                mp=$((mp + n))
            done < <(command grep 'test result:' "$mod_output" 2>/dev/null)
            total_passed=$((total_passed + mp))
            [[ $mod_exit -ne 0 ]] && total_failed=$((total_failed + 1))
            rm -f "$mod_output"
        done
        passed_count=$total_passed
        [[ $total_failed -gt 0 ]] && exit_code=1 || exit_code=0
    fi

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "reliability_suite_timing.log" "$(cat "$test_output")"
    e2e_add_file "reliability_timing.json" "{\"suite_seconds\": $duration_s, \"exit_code\": $exit_code, \"total_passed\": $passed_count}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.1: Reliability suite TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Reliability suite completed (${duration_s}s)"
    else
        log_fail "S7.1: Reliability suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    if [[ $duration_s -lt 60 ]]; then
        log_pass "S7.2: Suite within 60s budget (${duration_s}s)"
    else
        log_fail "S7.2: Suite exceeded 60s budget (${duration_s}s)"
    fi

    if [[ "$passed_count" -ge 50 ]]; then
        log_pass "S7.3: Reliability test count >= 50 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S7.3: Fewer reliability tests than expected ($passed_count, expected >= 50)"
    else
        log_fail "S7.3: Could not parse reliability test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Reliability Hardening Scenarios${NC}"
    echo -e "${BLUE}Bead: bd-1zgb${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "reliability-hardening" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "circuit_breaker" scenario_circuit_breaker || overall_exit=1
    e2e_capture_scenario "retry_logic" scenario_retry_logic || overall_exit=1
    e2e_capture_scenario "graceful_degradation" scenario_graceful_degradation || overall_exit=1
    e2e_capture_scenario "fault_injection" scenario_fault_injection || overall_exit=1
    e2e_capture_scenario "health_monitoring" scenario_health_monitoring || overall_exit=1
    e2e_capture_scenario "cross_module_integration" scenario_cross_module_integration || overall_exit=1
    e2e_capture_scenario "bounded_execution" scenario_bounded_execution || overall_exit=1

    e2e_finalize "$overall_exit" >/dev/null

    # Summary
    echo -e "\n${BLUE}================================================${NC}"
    echo -e "Results: ${GREEN}${TESTS_PASSED} passed${NC}, ${RED}${TESTS_FAILED} failed${NC}, ${YELLOW}${TESTS_SKIPPED} skipped${NC} (${TESTS_RUN} total)"
    echo -e "${BLUE}================================================${NC}"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo -e "${RED}OVERALL: FAIL${NC}"
        return 1
    fi

    echo -e "${GREEN}OVERALL: PASS${NC}"
    return 0
}

main "$@"
