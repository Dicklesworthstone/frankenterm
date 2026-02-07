#!/bin/bash
# =============================================================================
# E2E: Prioritized capture under load
# Implements: bd-3vo0
#
# Purpose:
#   Validate that the pane priority + capture budget system works correctly:
#   - CaptureScheduler weighted selection favors high-priority panes
#   - Global rate limits and byte budgets enforced
#   - Per-pane budget tracking with sliding windows
#   - Throttle events emitted and accumulated
#   - TailerSupervisor integration: priority ordering, budget hot-reload
#   - Pane priority config: TOML roundtrip, validation, hot-reload
#   - Overflow GAP emission under sustained backpressure
#   - Capture channel backpressure detection
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
# Scenario 1: CaptureScheduler weighted selection and budget enforcement
# ==============================================================================

scenario_scheduler_budget() {
    log_test "Scenario 1: CaptureScheduler weighted selection + budget enforcement"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'tailer::tests::scheduler' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "scheduler_budget.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: CaptureScheduler tests pass"
    else
        log_fail "S1.1: CaptureScheduler tests failed (exit=$exit_code)"
    fi

    # Verify weighted selection: high-priority panes always first
    if command grep -q "scheduler_high_priority_always_first" "$test_output"; then
        log_pass "S1.2: High-priority panes scheduled first"
    else
        log_fail "S1.2: High-priority scheduling test missing"
    fi

    # Verify mixed priorities under budget contention
    if command grep -q "scheduler_mixed_priorities_budget_favors_high" "$test_output"; then
        log_pass "S1.3: Budget favors high-priority under contention"
    else
        log_fail "S1.3: Mixed priorities budget test missing"
    fi

    # Verify global rate limiting
    if command grep -q "scheduler_global_rate_limits_captures" "$test_output"; then
        log_pass "S1.4: Global rate limiting enforced"
    else
        log_fail "S1.4: Global rate limiting test missing"
    fi

    # Verify byte budget exhaustion
    if command grep -q "scheduler_byte_budget_exhaustion" "$test_output"; then
        log_pass "S1.5: Byte budget exhaustion detected"
    else
        log_fail "S1.5: Byte budget exhaustion test missing"
    fi

    # Verify throttle events accumulate
    if command grep -q "scheduler_throttle_events_accumulate" "$test_output"; then
        log_pass "S1.6: Throttle events accumulate correctly"
    else
        log_fail "S1.6: Throttle event accumulation test missing"
    fi

    # Count scheduler tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 20 ]]; then
        log_pass "S1.7: $passed_count scheduler tests validated (>= 20)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S1.7: Only $passed_count scheduler tests (expected >= 20)"
    else
        log_fail "S1.7: Could not parse scheduler test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: Scheduler determinism and edge cases
# ==============================================================================

scenario_scheduler_determinism() {
    log_test "Scenario 2: Scheduler determinism and edge cases"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'tailer::tests::scheduler' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "scheduler_determinism.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: Scheduler determinism tests pass"
    else
        log_fail "S2.1: Scheduler determinism tests failed (exit=$exit_code)"
    fi

    # Equal priority deterministic by pane_id
    if command grep -q "scheduler_equal_priority_deterministic_by_pane_id" "$test_output"; then
        log_pass "S2.2: Equal-priority panes ordered deterministically by ID"
    else
        log_fail "S2.2: Deterministic ordering test missing"
    fi

    # Burst then exhaustion
    if command grep -q "scheduler_burst_then_exhaustion" "$test_output"; then
        log_pass "S2.3: Burst-then-exhaustion pattern validated"
    else
        log_fail "S2.3: Burst-then-exhaustion test missing"
    fi

    # Per-pane window reset
    if command grep -q "scheduler_per_pane_window_resets_after_one_second" "$test_output"; then
        log_pass "S2.4: Per-pane sliding window resets after 1s"
    else
        log_fail "S2.4: Per-pane window reset test missing"
    fi

    # Zero permits returns empty
    if command grep -q "scheduler_zero_permits_returns_empty" "$test_output"; then
        log_pass "S2.5: Zero permits returns empty selection"
    else
        log_fail "S2.5: Zero permits test missing"
    fi

    # Combined capture and byte budget
    if command grep -q "scheduler_combined_capture_and_byte_budget" "$test_output"; then
        log_pass "S2.6: Combined capture + byte budget respected"
    else
        log_fail "S2.6: Combined budget test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: TailerSupervisor priority and budget integration
# ==============================================================================

scenario_supervisor_integration() {
    log_test "Scenario 3: TailerSupervisor priority + budget integration"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'tailer::tests::supervisor' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "supervisor_integration.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: TailerSupervisor integration tests pass"
    else
        log_fail "S3.1: TailerSupervisor tests failed (exit=$exit_code)"
    fi

    # Supervisor spawns higher priority first
    if command grep -q "supervisor_spawns_higher_priority_panes_first" "$test_output"; then
        log_pass "S3.2: Supervisor spawns high-priority panes first"
    else
        log_fail "S3.2: Priority ordering test missing"
    fi

    # Supervisor with budget limits captures
    if command grep -q "supervisor_with_budget_limits_captures" "$test_output"; then
        log_pass "S3.3: Supervisor budget enforcement works"
    else
        log_fail "S3.3: Supervisor budget test missing"
    fi

    # Budget hot-reload
    if command grep -q "supervisor_budget_hot_reload" "$test_output"; then
        log_pass "S3.4: Budget hot-reload preserves state"
    else
        log_fail "S3.4: Budget hot-reload test missing"
    fi

    # Backpressure records timeout
    if command grep -q "supervisor_backpressure_records_timeout" "$test_output"; then
        log_pass "S3.5: Backpressure timeout recorded"
    else
        log_fail "S3.5: Backpressure timeout test missing"
    fi

    # Changed outcome records bytes
    if command grep -q "supervisor_changed_outcome_records_bytes" "$test_output"; then
        log_pass "S3.6: Capture bytes recorded on changed outcome"
    else
        log_fail "S3.6: Bytes recording test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Overflow GAP emission under sustained load
# ==============================================================================

scenario_overflow_gap() {
    log_test "Scenario 4: Overflow GAP emission under sustained load"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'overflow' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "overflow_gap.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Overflow GAP tests pass"
    else
        log_fail "S4.1: Overflow GAP tests failed (exit=$exit_code)"
    fi

    # Verify overflow threshold is reasonable
    if command grep -q "overflow_threshold_constant_is_reasonable" "$test_output"; then
        log_pass "S4.2: Overflow threshold constant validated"
    else
        log_fail "S4.2: Overflow threshold test missing"
    fi

    # GAP emitted clears pending flag
    if command grep -q "overflow_gap_emitted_clears_pending_flag" "$test_output"; then
        log_pass "S4.3: GAP emission clears pending flag"
    else
        log_fail "S4.3: GAP clearing test missing"
    fi

    # GAP advances cursor sequence
    if command grep -q "overflow_gap_advances_cursor_seq" "$test_output"; then
        log_pass "S4.4: GAP advances cursor sequence"
    else
        log_fail "S4.4: GAP cursor advance test missing"
    fi

    # Count overflow tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 8 ]]; then
        log_pass "S4.5: $passed_count overflow tests validated (>= 8)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S4.5: Only $passed_count overflow tests (expected >= 8)"
    else
        log_fail "S4.5: Could not parse overflow test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Pane priority config + validation
# ==============================================================================

scenario_priority_config() {
    log_test "Scenario 5: Pane priority config + validation"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'pane_priority' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "priority_config.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Pane priority config tests pass"
    else
        log_fail "S5.1: Pane priority config tests failed (exit=$exit_code)"
    fi

    # TOML roundtrip
    if command grep -q "pane_priority_and_budget_toml_roundtrip" "$test_output"; then
        log_pass "S5.2: TOML roundtrip for priorities + budgets"
    else
        log_fail "S5.2: TOML roundtrip test missing"
    fi

    # Validation rejects duplicates
    if command grep -q "pane_priority_validation_duplicate_ids" "$test_output"; then
        log_pass "S5.3: Duplicate rule ID validation"
    else
        log_fail "S5.3: Duplicate ID validation test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Capture channel backpressure detection
# ==============================================================================

scenario_capture_backpressure() {
    log_test "Scenario 6: Capture channel backpressure detection"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'capture_channel' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "capture_backpressure.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Capture channel backpressure tests pass"
    else
        log_fail "S6.1: Capture channel backpressure tests failed (exit=$exit_code)"
    fi

    # Backpressure detected
    if command grep -q "capture_channel_backpressure_detected" "$test_output"; then
        log_pass "S6.2: Backpressure detected on saturated channel"
    else
        log_fail "S6.2: Backpressure detection test missing"
    fi

    # Channel drains when consumer resumes
    if command grep -q "capture_channel_drains_when_consumer_resumes" "$test_output"; then
        log_pass "S6.3: Channel drains when consumer resumes"
    else
        log_fail "S6.3: Channel drain test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Health snapshot includes scheduler state
# ==============================================================================

scenario_health_scheduler() {
    log_test "Scenario 7: Health snapshot includes scheduler state"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'health_snapshot' \
        --no-fail-fast -- --nocapture --skip health_json_schema \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "health_scheduler.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Health snapshot scheduler tests pass"
    else
        log_fail "S7.1: Health snapshot scheduler tests failed (exit=$exit_code)"
    fi

    # Scheduler included in snapshot
    if command grep -q "health_snapshot_includes_scheduler_when_active" "$test_output"; then
        log_pass "S7.2: Scheduler state included in health snapshot"
    else
        log_fail "S7.2: Scheduler-in-snapshot test missing"
    fi

    # Scheduler serialization roundtrip
    if command grep -q "health_snapshot_scheduler_serializes_roundtrip" "$test_output"; then
        log_pass "S7.3: Scheduler snapshot serializes roundtrip"
    else
        log_fail "S7.3: Scheduler serialization test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 8: Bounded execution (full priority + capture test suite timing)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 8: Bounded execution (priority + capture suite timing)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    # Run all scheduler, supervisor, overflow, priority, and capture tests
    timeout 90 cargo test -p wa-core 'tailer::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "tailer_suite_timing.log" "$(cat "$test_output")"
    e2e_add_file "tailer_timing.json" "{\"tailer_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S8.1: Tailer test suite TIMED OUT after 90s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Tailer test suite completed (${duration_s}s)"
    else
        log_fail "S8.1: Tailer test suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Suite should complete within 45s
    if [[ $duration_s -lt 45 ]]; then
        log_pass "S8.2: Tailer suite within 45s budget (${duration_s}s)"
    else
        log_fail "S8.2: Tailer suite exceeded 45s budget (${duration_s}s)"
    fi

    # Count total tailer tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 30 ]]; then
        log_pass "S8.3: Tailer test count >= 30 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S8.3: Fewer tailer tests than expected ($passed_count, expected >= 30)"
    else
        log_fail "S8.3: Could not parse tailer test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Prioritized Capture Under Load${NC}"
    echo -e "${BLUE}Bead: bd-3vo0${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "prioritized-capture" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "scheduler_budget" scenario_scheduler_budget || overall_exit=1
    e2e_capture_scenario "scheduler_determinism" scenario_scheduler_determinism || overall_exit=1
    e2e_capture_scenario "supervisor_integration" scenario_supervisor_integration || overall_exit=1
    e2e_capture_scenario "overflow_gap" scenario_overflow_gap || overall_exit=1
    e2e_capture_scenario "priority_config" scenario_priority_config || overall_exit=1
    e2e_capture_scenario "capture_backpressure" scenario_capture_backpressure || overall_exit=1
    e2e_capture_scenario "health_scheduler" scenario_health_scheduler || overall_exit=1
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
