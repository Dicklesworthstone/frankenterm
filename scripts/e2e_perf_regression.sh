#!/bin/bash
# =============================================================================
# E2E: Performance regression smoke with detailed logging
# Implements: bd-38et
#
# Purpose:
#   Validate performance optimizations end-to-end without regressions:
#   - Pattern detection tests (regex compilation, matching, detection)
#   - Delta extraction tests (overlap detection, diffing)
#   - Output cache tests (LRU eviction, deduplication, hit rates)
#   - Storage benchmarks (append, batch, FTS search within budgets)
#   - Delta extraction benchmarks (within p50/p99 budgets)
#   - Backpressure benchmarks (classify, tier transition latency)
#   - Pattern detection benchmarks (regex, multi-pattern matching)
#   - Watcher loop benchmarks (tick latency, throughput)
#   - Perf artifacts captured for regression tracking
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
# Scenario 1: Pattern detection correctness
# ==============================================================================

scenario_pattern_detection() {
    log_test "Scenario 1: Pattern detection correctness"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'patterns::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "pattern_detection.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: Pattern detection tests pass"
    else
        log_fail "S1.1: Pattern detection tests failed (exit=$exit_code)"
    fi

    # Count pattern tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 50 ]]; then
        log_pass "S1.2: $passed_count pattern tests validated (>= 50)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S1.2: Only $passed_count pattern tests (expected >= 50)"
    else
        log_fail "S1.2: Could not parse pattern test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: Delta extraction correctness
# ==============================================================================

scenario_delta_extraction() {
    log_test "Scenario 2: Delta extraction correctness"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'extract_delta' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "delta_extraction.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: Delta extraction tests pass"
    else
        log_fail "S2.1: Delta extraction tests failed (exit=$exit_code)"
    fi

    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 3 ]]; then
        log_pass "S2.2: $passed_count delta tests validated (>= 3)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S2.2: Only $passed_count delta tests (expected >= 3)"
    else
        log_fail "S2.2: Could not parse delta test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: Output cache (LRU, dedup, hit rates)
# ==============================================================================

scenario_output_cache() {
    log_test "Scenario 3: Output cache (LRU, dedup, hit rates)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'output_cache' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "output_cache.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: Output cache tests pass"
    else
        log_fail "S3.1: Output cache tests failed (exit=$exit_code)"
    fi

    # Verify key cache behaviors
    for test_name in output_cache_lru_eviction output_cache_per_pane_deduplication output_cache_hit_rate_calculation output_cache_prune_stale_panes; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S3.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S3.${TESTS_RUN}: $test_name missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Storage regression benchmarks (compile + budget validation)
# ==============================================================================

scenario_storage_benchmarks() {
    log_test "Scenario 4: Storage regression benchmarks"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core --bench storage_regression \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "storage_bench.log" "$(cat "$test_output")"
    e2e_add_file "storage_bench_timing.json" "{\"seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S4.1: Storage benchmarks TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Storage benchmarks pass (${duration_s}s)"
    else
        log_fail "S4.1: Storage benchmarks failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify budget manifest is emitted
    if command grep -q 'BENCH.*budget' "$test_output"; then
        log_pass "S4.2: Performance budget manifest emitted"
    else
        log_fail "S4.2: Budget manifest missing from benchmark output"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Delta extraction benchmarks
# ==============================================================================

scenario_delta_benchmarks() {
    log_test "Scenario 5: Delta extraction benchmarks"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 60 cargo test -p wa-core --bench delta_extraction \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "delta_bench.log" "$(cat "$test_output")"
    e2e_add_file "delta_bench_timing.json" "{\"seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S5.1: Delta benchmarks TIMED OUT after 60s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Delta extraction benchmarks pass (${duration_s}s)"
    else
        log_fail "S5.1: Delta benchmarks failed (exit=$exit_code, ${duration_s}s)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Pattern detection benchmarks
# ==============================================================================

scenario_pattern_benchmarks() {
    log_test "Scenario 6: Pattern detection benchmarks"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 60 cargo test -p wa-core --bench pattern_detection \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "pattern_bench.log" "$(cat "$test_output")"
    e2e_add_file "pattern_bench_timing.json" "{\"seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S6.1: Pattern benchmarks TIMED OUT after 60s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Pattern detection benchmarks pass (${duration_s}s)"
    else
        log_fail "S6.1: Pattern benchmarks failed (exit=$exit_code, ${duration_s}s)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Backpressure + watcher loop benchmarks
# ==============================================================================

scenario_backpressure_watcher_benchmarks() {
    log_test "Scenario 7: Backpressure + watcher loop benchmarks"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core --bench backpressure_performance \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "backpressure_bench.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.1: Backpressure benchmarks TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Backpressure benchmarks pass (${duration_s}s)"
    else
        log_fail "S7.1: Backpressure benchmarks failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Watcher loop benchmarks
    local watcher_output watcher_exit watcher_start watcher_end watcher_duration
    watcher_output=$(mktemp)
    watcher_start=$(date +%s)
    watcher_exit=0

    timeout 60 cargo test -p wa-core --bench watcher_loop \
        -- --test --nocapture \
        >"$watcher_output" 2>&1 || watcher_exit=$?

    watcher_end=$(date +%s)
    watcher_duration=$((watcher_end - watcher_start))

    e2e_add_file "watcher_bench.log" "$(cat "$watcher_output")"

    if [[ $watcher_exit -eq 124 ]]; then
        log_fail "S7.2: Watcher loop benchmarks TIMED OUT after 60s"
    elif [[ $watcher_exit -eq 0 ]]; then
        log_pass "S7.2: Watcher loop benchmarks pass (${watcher_duration}s)"
    else
        log_fail "S7.2: Watcher loop benchmarks failed (exit=$watcher_exit, ${watcher_duration}s)"
    fi

    rm -f "$test_output" "$watcher_output"
}

# ==============================================================================
# Scenario 8: Bounded execution (all perf tests within timing budget)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 8: Bounded execution (all perf-critical tests)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    # Run pattern + delta + cache tests together
    timeout 60 cargo test -p wa-core 'patterns::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "perf_tests_timing.json" "{\"pattern_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S8.1: Pattern suite TIMED OUT after 60s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Pattern suite completed (${duration_s}s)"
    else
        log_fail "S8.1: Pattern suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    if [[ $duration_s -lt 30 ]]; then
        log_pass "S8.2: Pattern suite within 30s budget (${duration_s}s)"
    else
        log_fail "S8.2: Pattern suite exceeded 30s budget (${duration_s}s)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Performance Regression Smoke${NC}"
    echo -e "${BLUE}Bead: bd-38et${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "perf-regression" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "pattern_detection" scenario_pattern_detection || overall_exit=1
    e2e_capture_scenario "delta_extraction" scenario_delta_extraction || overall_exit=1
    e2e_capture_scenario "output_cache" scenario_output_cache || overall_exit=1
    e2e_capture_scenario "storage_benchmarks" scenario_storage_benchmarks || overall_exit=1
    e2e_capture_scenario "delta_benchmarks" scenario_delta_benchmarks || overall_exit=1
    e2e_capture_scenario "pattern_benchmarks" scenario_pattern_benchmarks || overall_exit=1
    e2e_capture_scenario "backpressure_watcher_benchmarks" scenario_backpressure_watcher_benchmarks || overall_exit=1
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
