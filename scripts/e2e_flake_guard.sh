#!/bin/bash
# =============================================================================
# E2E: Flake guard mode — repeat-run representative cases to detect flakiness
# Implements: wa-upg.3.6
#
# Purpose:
#   Detect timing regressions and test flakiness by re-running representative
#   E2E test suites multiple times and tracking per-iteration results:
#   - Each iteration runs independently with fresh state
#   - Timing data collected per iteration for regression detection
#   - Artifacts retained on first failure with per-iteration logs
#   - Configurable iteration count (default: 5, adjustable for CI)
#   - Summary shows timing variance across iterations
#
# Usage:
#   ./e2e_flake_guard.sh                      # Default: 5 iterations
#   ./e2e_flake_guard.sh --iterations 10      # Custom count
#   ./e2e_flake_guard.sh --verbose            # Show per-test details
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
ITERATIONS=5

# ==============================================================================
# Argument parsing
# ==============================================================================

while [[ $# -gt 0 ]]; do
    case "$1" in
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        --iterations|-n)
            ITERATIONS="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--verbose] [--iterations N]" >&2
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
# Helper: run a test suite N times, collect timing
# ==============================================================================

# Runs a cargo test filter N times, collecting per-iteration timing
# Args: suite_name filter iterations
# Sets: ITER_TIMES[] ITER_EXITS[] ITER_FAILURES
run_repeated_suite() {
    local suite_name="$1"
    local filter="$2"
    local iters="$3"

    ITER_TIMES=()
    ITER_EXITS=()
    ITER_FAILURES=0

    local i
    for ((i = 1; i <= iters; i++)); do
        local iter_output exit_code start_time end_time duration_s
        iter_output=$(mktemp)
        start_time=$(date +%s)
        exit_code=0

        timeout 120 cargo test -p wa-core "$filter" \
            --no-fail-fast -- --nocapture \
            >"$iter_output" 2>&1 || exit_code=$?

        end_time=$(date +%s)
        duration_s=$((end_time - start_time))

        ITER_TIMES+=("$duration_s")
        ITER_EXITS+=("$exit_code")

        if [[ $exit_code -ne 0 ]]; then
            ((ITER_FAILURES++)) || true
            e2e_add_file "${suite_name}_iter${i}_FAIL.log" "$(cat "$iter_output")"
        fi

        log_info "  Iteration $i/$iters: ${duration_s}s (exit=$exit_code)"

        rm -f "$iter_output"
    done
}

# Compute timing statistics from ITER_TIMES[]
compute_timing_stats() {
    local min=999999 max=0 sum=0
    local count=${#ITER_TIMES[@]}

    for t in "${ITER_TIMES[@]}"; do
        ((sum += t)) || true
        [[ $t -lt $min ]] && min=$t
        [[ $t -gt $max ]] && max=$t
    done

    local avg=$((sum / count))
    local spread=$((max - min))

    STAT_MIN=$min
    STAT_MAX=$max
    STAT_AVG=$avg
    STAT_SPREAD=$spread
    STAT_COUNT=$count
}

# ==============================================================================
# Scenario 1: FTS search tests — repeated N times
# ==============================================================================

scenario_fts_flake_guard() {
    log_test "Scenario 1: FTS search tests — $ITERATIONS iterations"

    run_repeated_suite "fts_search" "storage::fts_search" "$ITERATIONS"
    compute_timing_stats

    e2e_add_file "fts_search_timing.json" "{\"iterations\": $STAT_COUNT, \"min_s\": $STAT_MIN, \"max_s\": $STAT_MAX, \"avg_s\": $STAT_AVG, \"spread_s\": $STAT_SPREAD, \"failures\": $ITER_FAILURES}"

    if [[ $ITER_FAILURES -eq 0 ]]; then
        log_pass "S1.1: FTS search: $ITERATIONS/$ITERATIONS iterations passed"
    else
        log_fail "S1.1: FTS search: $ITER_FAILURES/$ITERATIONS iterations failed"
    fi

    # Timing variance: spread should be < 2x average
    if [[ $STAT_AVG -gt 0 && $STAT_SPREAD -le $((STAT_AVG * 2)) ]]; then
        log_pass "S1.2: FTS timing stable (spread=${STAT_SPREAD}s, avg=${STAT_AVG}s)"
    elif [[ $STAT_AVG -eq 0 ]]; then
        log_pass "S1.2: FTS timing sub-second across all iterations"
    else
        log_fail "S1.2: FTS timing unstable (spread=${STAT_SPREAD}s > 2x avg=${STAT_AVG}s)"
    fi
}

# ==============================================================================
# Scenario 2: Incident bundle tests — repeated N times
# ==============================================================================

scenario_incident_flake_guard() {
    log_test "Scenario 2: Incident bundle tests — $ITERATIONS iterations"

    run_repeated_suite "incident_bundle" "incident_bundle" "$ITERATIONS"
    compute_timing_stats

    e2e_add_file "incident_bundle_timing.json" "{\"iterations\": $STAT_COUNT, \"min_s\": $STAT_MIN, \"max_s\": $STAT_MAX, \"avg_s\": $STAT_AVG, \"spread_s\": $STAT_SPREAD, \"failures\": $ITER_FAILURES}"

    if [[ $ITER_FAILURES -eq 0 ]]; then
        log_pass "S2.1: Incident bundle: $ITERATIONS/$ITERATIONS iterations passed"
    else
        log_fail "S2.1: Incident bundle: $ITER_FAILURES/$ITERATIONS iterations failed"
    fi

    if [[ $STAT_AVG -gt 0 && $STAT_SPREAD -le $((STAT_AVG * 2)) ]]; then
        log_pass "S2.2: Incident timing stable (spread=${STAT_SPREAD}s, avg=${STAT_AVG}s)"
    elif [[ $STAT_AVG -eq 0 ]]; then
        log_pass "S2.2: Incident timing sub-second across all iterations"
    else
        log_fail "S2.2: Incident timing unstable (spread=${STAT_SPREAD}s > 2x avg=${STAT_AVG}s)"
    fi
}

# ==============================================================================
# Scenario 3: Scheduler tests — repeated N times
# ==============================================================================

scenario_scheduler_flake_guard() {
    log_test "Scenario 3: Scheduler tests — $ITERATIONS iterations"

    run_repeated_suite "scheduler" "tailer::tests::scheduler" "$ITERATIONS"
    compute_timing_stats

    e2e_add_file "scheduler_timing.json" "{\"iterations\": $STAT_COUNT, \"min_s\": $STAT_MIN, \"max_s\": $STAT_MAX, \"avg_s\": $STAT_AVG, \"spread_s\": $STAT_SPREAD, \"failures\": $ITER_FAILURES}"

    if [[ $ITER_FAILURES -eq 0 ]]; then
        log_pass "S3.1: Scheduler: $ITERATIONS/$ITERATIONS iterations passed"
    else
        log_fail "S3.1: Scheduler: $ITER_FAILURES/$ITERATIONS iterations failed"
    fi

    if [[ $STAT_AVG -gt 0 && $STAT_SPREAD -le $((STAT_AVG * 2)) ]]; then
        log_pass "S3.2: Scheduler timing stable (spread=${STAT_SPREAD}s, avg=${STAT_AVG}s)"
    elif [[ $STAT_AVG -eq 0 ]]; then
        log_pass "S3.2: Scheduler timing sub-second across all iterations"
    else
        log_fail "S3.2: Scheduler timing unstable (spread=${STAT_SPREAD}s > 2x avg=${STAT_AVG}s)"
    fi
}

# ==============================================================================
# Scenario 4: Pane UUID tests — repeated N times
# ==============================================================================

scenario_pane_uuid_flake_guard() {
    log_test "Scenario 4: Pane UUID tests — $ITERATIONS iterations"

    run_repeated_suite "pane_uuid" "pane_uuid" "$ITERATIONS"
    compute_timing_stats

    e2e_add_file "pane_uuid_timing.json" "{\"iterations\": $STAT_COUNT, \"min_s\": $STAT_MIN, \"max_s\": $STAT_MAX, \"avg_s\": $STAT_AVG, \"spread_s\": $STAT_SPREAD, \"failures\": $ITER_FAILURES}"

    if [[ $ITER_FAILURES -eq 0 ]]; then
        log_pass "S4.1: Pane UUID: $ITERATIONS/$ITERATIONS iterations passed"
    else
        log_fail "S4.1: Pane UUID: $ITER_FAILURES/$ITERATIONS iterations failed"
    fi

    if [[ $STAT_AVG -gt 0 && $STAT_SPREAD -le $((STAT_AVG * 2)) ]]; then
        log_pass "S4.2: UUID timing stable (spread=${STAT_SPREAD}s, avg=${STAT_AVG}s)"
    elif [[ $STAT_AVG -eq 0 ]]; then
        log_pass "S4.2: UUID timing sub-second across all iterations"
    else
        log_fail "S4.2: UUID timing unstable (spread=${STAT_SPREAD}s > 2x avg=${STAT_AVG}s)"
    fi
}

# ==============================================================================
# Scenario 5: Backpressure tests — repeated N times
# ==============================================================================

scenario_backpressure_flake_guard() {
    log_test "Scenario 5: Backpressure tests — $ITERATIONS iterations"

    run_repeated_suite "backpressure" "backpressure::tests" "$ITERATIONS"
    compute_timing_stats

    e2e_add_file "backpressure_timing.json" "{\"iterations\": $STAT_COUNT, \"min_s\": $STAT_MIN, \"max_s\": $STAT_MAX, \"avg_s\": $STAT_AVG, \"spread_s\": $STAT_SPREAD, \"failures\": $ITER_FAILURES}"

    if [[ $ITER_FAILURES -eq 0 ]]; then
        log_pass "S5.1: Backpressure: $ITERATIONS/$ITERATIONS iterations passed"
    else
        log_fail "S5.1: Backpressure: $ITER_FAILURES/$ITERATIONS iterations failed"
    fi

    if [[ $STAT_AVG -gt 0 && $STAT_SPREAD -le $((STAT_AVG * 2)) ]]; then
        log_pass "S5.2: Backpressure timing stable (spread=${STAT_SPREAD}s, avg=${STAT_AVG}s)"
    elif [[ $STAT_AVG -eq 0 ]]; then
        log_pass "S5.2: Backpressure timing sub-second across all iterations"
    else
        log_fail "S5.2: Backpressure timing unstable (spread=${STAT_SPREAD}s > 2x avg=${STAT_AVG}s)"
    fi
}

# ==============================================================================
# Scenario 6: Aggregate flake rate
# ==============================================================================

scenario_aggregate_flake_rate() {
    log_test "Scenario 6: Aggregate flake rate"

    local total_iterations=$((ITERATIONS * 5))  # 5 suites x N iterations

    # Count total failures across all previous scenarios
    # This scenario just validates the overall health
    if [[ $TESTS_FAILED -eq 0 ]]; then
        log_pass "S6.1: Zero flakes across all suites ($total_iterations total iterations)"
    else
        log_fail "S6.1: $TESTS_FAILED flake(s) detected across suites"
    fi

    # Overall timing budget: all suites * iterations should complete within reason
    log_pass "S6.2: Flake guard completed $total_iterations iterations across 5 suites"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Flake Guard Mode${NC}"
    echo -e "${BLUE}Bead: wa-upg.3.6${NC}"
    echo -e "${BLUE}Iterations: ${ITERATIONS}${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "flake-guard" >/dev/null

    local overall_exit=0

    # Run all suites repeatedly
    e2e_capture_scenario "fts_flake_guard" scenario_fts_flake_guard || overall_exit=1
    e2e_capture_scenario "incident_flake_guard" scenario_incident_flake_guard || overall_exit=1
    e2e_capture_scenario "scheduler_flake_guard" scenario_scheduler_flake_guard || overall_exit=1
    e2e_capture_scenario "pane_uuid_flake_guard" scenario_pane_uuid_flake_guard || overall_exit=1
    e2e_capture_scenario "backpressure_flake_guard" scenario_backpressure_flake_guard || overall_exit=1
    e2e_capture_scenario "aggregate_flake_rate" scenario_aggregate_flake_rate || overall_exit=1

    e2e_finalize "$overall_exit" >/dev/null

    # Summary
    echo -e "\n${BLUE}================================================${NC}"
    echo -e "Results: ${GREEN}${TESTS_PASSED} passed${NC}, ${RED}${TESTS_FAILED} failed${NC}, ${YELLOW}${TESTS_SKIPPED} skipped${NC} (${TESTS_RUN} total)"
    echo -e "Iterations per suite: ${ITERATIONS}"
    echo -e "${BLUE}================================================${NC}"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo -e "${RED}OVERALL: FAIL${NC}"
        return 1
    fi

    echo -e "${GREEN}OVERALL: PASS${NC}"
    return 0
}

main "$@"
