#!/bin/bash
# =============================================================================
# E2E: Large transcript search performance (verbose perf artifacts)
# Implements: wa-upg.5.6
#
# Purpose:
#   Validate that FTS search remains fast as transcript volume grows:
#   - FTS correctness tests pass at all scales
#   - FTS benchmarks compile and run (test mode) at 1K/10K/100K segments
#   - Storage regression benchmarks validate write + search budgets
#   - DB sizing benchmarks validate growth rate stays bounded
#   - Perf artifacts are captured for regression tracking
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
# Scenario 1: FTS search correctness (all query types)
# ==============================================================================

scenario_fts_search_correctness() {
    log_test "Scenario 1: FTS search correctness (all query types)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'storage::fts_search' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fts_search_correctness.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: FTS search correctness tests pass"
    else
        log_fail "S1.1: FTS search tests failed (exit=$exit_code)"
    fi

    # Verify key search behaviors are tested
    for test_name in bm25_ordering respects_limit respects_pane_filter respects_time_filter returns_snippets_with_highlights; do
        if command grep -q "fts_search_$test_name" "$test_output"; then
            log_pass "S1.${TESTS_RUN}: $test_name tested"
        else
            log_fail "S1.${TESTS_RUN}: $test_name test missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: FTS sync pipeline (incremental indexing)
# ==============================================================================

scenario_fts_sync_pipeline() {
    log_test "Scenario 2: FTS sync pipeline (incremental indexing)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'storage::fts_sync_tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fts_sync_pipeline.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: FTS sync pipeline tests pass"
    else
        log_fail "S2.1: FTS sync pipeline failed (exit=$exit_code)"
    fi

    # Key incremental sync behaviors
    if command grep -q "sync_fts_respects_progress" "$test_output"; then
        log_pass "S2.2: Incremental sync respects per-pane progress"
    else
        log_fail "S2.2: Incremental sync progress test missing"
    fi

    if command grep -q "batch_config_limits_work" "$test_output"; then
        log_pass "S2.3: Batch config limits work"
    else
        log_fail "S2.3: Batch config limits test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: FTS query benchmarks (1K/10K/100K segments)
# ==============================================================================

scenario_fts_benchmark_scales() {
    log_test "Scenario 3: FTS query benchmarks at scale"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    # Run FTS benchmarks in test mode (validates correct operation at each scale)
    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core --bench fts_query \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "fts_benchmark_test.log" "$(cat "$test_output")"
    e2e_add_file "fts_benchmark_timing.json" "{\"fts_bench_test_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S3.1: FTS benchmark test TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: FTS benchmarks pass at all scales (${duration_s}s)"
    else
        log_fail "S3.1: FTS benchmark test failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify specific scale groups ran
    if command grep -q "fts_small_db" "$test_output"; then
        log_pass "S3.2: 1K segment scale tested"
    else
        log_fail "S3.2: 1K segment scale missing"
    fi

    if command grep -q "fts_medium_db" "$test_output"; then
        log_pass "S3.3: 10K segment scale tested"
    else
        log_fail "S3.3: 10K segment scale missing"
    fi

    if command grep -q "fts_large_db" "$test_output"; then
        log_pass "S3.4: 100K segment scale tested"
    else
        log_fail "S3.4: 100K segment scale missing"
    fi

    if command grep -q "fts_result_limits" "$test_output"; then
        log_pass "S3.5: Result limit scaling tested"
    else
        log_fail "S3.5: Result limit scaling missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Storage write + search regression benchmarks
# ==============================================================================

scenario_storage_regression_benchmarks() {
    log_test "Scenario 4: Storage write + search regression benchmarks"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core --bench storage_regression \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "storage_regression_test.log" "$(cat "$test_output")"
    e2e_add_file "storage_regression_timing.json" "{\"storage_bench_test_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S4.1: Storage regression bench TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Storage regression benchmarks pass (${duration_s}s)"
    else
        log_fail "S4.1: Storage regression bench failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify key benchmark groups present
    if command grep -q "single_append" "$test_output" || command grep -q "append_segment" "$test_output"; then
        log_pass "S4.2: Append segment benchmark present"
    else
        log_fail "S4.2: Append benchmark missing"
    fi

    if command grep -q "batch_append" "$test_output"; then
        log_pass "S4.3: Batch append throughput benchmark present"
    else
        log_fail "S4.3: Batch append benchmark missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: DB sizing benchmarks (growth rate validation)
# ==============================================================================

scenario_db_sizing_benchmarks() {
    log_test "Scenario 5: DB sizing benchmarks (growth rate)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 180 cargo test -p wa-core --bench sizing_benchmark \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "sizing_benchmark_test.log" "$(cat "$test_output")"
    e2e_add_file "sizing_benchmark_timing.json" "{\"sizing_bench_test_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S5.1: Sizing benchmark TIMED OUT after 180s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Sizing benchmarks pass (${duration_s}s)"
    else
        log_fail "S5.1: Sizing benchmark failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify multi-pane scale groups
    if command grep -q "multi_pane" "$test_output" || command grep -q "insert_throughput" "$test_output"; then
        log_pass "S5.2: Multi-pane sizing tested"
    else
        log_fail "S5.2: Multi-pane sizing missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Search linting and verify/rebuild tests
# ==============================================================================

scenario_search_linting() {
    log_test "Scenario 6: Search linting and FTS maintenance"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run search linting tests if they exist
    cargo test -p wa-core 'search' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "search_linting.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Search-related tests pass"
    else
        log_fail "S6.1: Search tests failed (exit=$exit_code)"
    fi

    # Count search tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -gt 0 ]]; then
        log_pass "S6.2: $passed_count search-related tests validated"
    else
        log_skip "S6.2: No search tests matched filter"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Bounded execution (all FTS + search within timing budget)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 7: Bounded execution (FTS test suite timing)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 60 cargo test -p wa-core 'storage::fts' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "fts_suite_timing.log" "$(cat "$test_output")"
    e2e_add_file "fts_timing.json" "{\"fts_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.1: FTS test suite TIMED OUT after 60s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: FTS test suite completed (${duration_s}s)"
    else
        log_fail "S7.1: FTS test suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Suite should complete well within 30s
    if [[ $duration_s -lt 30 ]]; then
        log_pass "S7.2: FTS suite within 30s budget (${duration_s}s)"
    else
        log_fail "S7.2: FTS suite exceeded 30s budget (${duration_s}s)"
    fi

    # Count total FTS tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 20 ]]; then
        log_pass "S7.3: FTS test count >= 20 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S7.3: Fewer FTS tests than expected ($passed_count, expected >= 20)"
    else
        log_fail "S7.3: Could not parse FTS test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Large Transcript Search Performance${NC}"
    echo -e "${BLUE}Bead: wa-upg.5.6${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "search-perf" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "fts_search_correctness" scenario_fts_search_correctness || overall_exit=1
    e2e_capture_scenario "fts_sync_pipeline" scenario_fts_sync_pipeline || overall_exit=1
    e2e_capture_scenario "fts_benchmark_scales" scenario_fts_benchmark_scales || overall_exit=1
    e2e_capture_scenario "storage_regression_benchmarks" scenario_storage_regression_benchmarks || overall_exit=1
    e2e_capture_scenario "db_sizing_benchmarks" scenario_db_sizing_benchmarks || overall_exit=1
    e2e_capture_scenario "search_linting" scenario_search_linting || overall_exit=1
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
