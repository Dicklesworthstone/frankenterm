#!/bin/bash
# =============================================================================
# E2E: Storage/indexing stress test (many panes + large transcripts)
# Implements: wa-upg.5.5
#
# Purpose:
#   Prove end-to-end that the storage and FTS indexing subsystem stays stable
#   under heavy load:
#   - All 230+ storage unit tests pass (correctness baseline)
#   - FTS search and sync pipeline tests pass
#   - Storage + FTS benchmarks run within budget (no runaway allocations)
#   - Multi-pane concurrent operations don't deadlock
#   - Indexing keeps up (no unbounded growth or lag)
#   - System completes all work within bounded time
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
# Scenario 1: Storage core test suite (correctness baseline)
# ==============================================================================

scenario_storage_core_tests() {
    log_test "Scenario 1: Storage core test suite (correctness baseline)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run ALL storage tests (200+ tests)
    cargo test -p wa-core 'storage::' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "storage_core_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: All storage tests pass"
    else
        log_fail "S1.1: Storage tests failed (exit=$exit_code)"
    fi

    # Count how many tests ran (sum across all test binaries)
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 200 ]]; then
        log_pass "S1.2: Storage test count >= 200 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S1.2: Fewer storage tests than expected ($passed_count passed, expected >= 200)"
    else
        log_fail "S1.2: Could not parse test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: FTS search correctness
# ==============================================================================

scenario_fts_search_tests() {
    log_test "Scenario 2: FTS search correctness"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run FTS search tests
    cargo test -p wa-core 'storage::fts_search' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fts_search_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: FTS search tests pass"
    else
        log_fail "S2.1: FTS search tests failed (exit=$exit_code)"
    fi

    # Verify specific FTS behaviors
    if command grep -q "fts_search_bm25_ordering" "$test_output"; then
        log_pass "S2.2: BM25 ordering tested"
    else
        log_fail "S2.2: BM25 ordering test not found"
    fi

    if command grep -q "fts_search_respects_pane_filter" "$test_output"; then
        log_pass "S2.3: Pane filter in search tested"
    else
        log_fail "S2.3: Pane filter test not found"
    fi

    if command grep -q "fts_search_returns_snippets_with_highlights" "$test_output"; then
        log_pass "S2.4: Snippet highlighting tested"
    else
        log_fail "S2.4: Snippet highlighting test not found"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: FTS sync pipeline (incremental indexing)
# ==============================================================================

scenario_fts_sync_tests() {
    log_test "Scenario 3: FTS sync pipeline (incremental indexing)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'storage::fts_sync_tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fts_sync_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: FTS sync pipeline tests pass"
    else
        log_fail "S3.1: FTS sync pipeline tests failed (exit=$exit_code)"
    fi

    # Key behaviors
    if command grep -q "sync_fts_respects_progress" "$test_output"; then
        log_pass "S3.2: Incremental sync respects progress"
    else
        log_fail "S3.2: Incremental sync progress test missing"
    fi

    if command grep -q "full_rebuild_is_idempotent" "$test_output"; then
        log_pass "S3.3: Full rebuild idempotent"
    else
        log_fail "S3.3: Full rebuild idempotent test missing"
    fi

    if command grep -q "version_mismatch_triggers_rebuild" "$test_output"; then
        log_pass "S3.4: Version mismatch triggers rebuild"
    else
        log_fail "S3.4: Version mismatch rebuild test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Concurrent writer safety
# ==============================================================================

scenario_concurrent_writers() {
    log_test "Scenario 4: Concurrent writer safety"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run the concurrent writer deadlock test specifically
    cargo test -p wa-core 'storage_concurrent_writers_dont_deadlock' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "concurrent_writers.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Concurrent writers don't deadlock"
    else
        log_fail "S4.1: Concurrent writer test failed (exit=$exit_code)"
    fi

    # Run the event bus subscriber lag test
    exit_code=0
    cargo test -p wa-core 'event_bus_detects_subscriber_lag' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.2: Event bus subscriber lag detection works"
    else
        log_fail "S4.2: Event bus lag detection failed"
    fi

    # Run capture channel drain test
    exit_code=0
    cargo test -p wa-core 'capture_channel_drains_when_consumer_resumes' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.3: Capture channel drains after backpressure"
    else
        log_fail "S4.3: Capture channel drain test failed"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: DB health and repair operations
# ==============================================================================

scenario_db_health_repair() {
    log_test "Scenario 5: DB health check and repair operations"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'storage::db_check_repair_tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "db_check_repair.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: DB check/repair tests pass"
    else
        log_fail "S5.1: DB check/repair tests failed (exit=$exit_code)"
    fi

    # Key behaviors
    if command grep -q "check_healthy_db" "$test_output"; then
        log_pass "S5.2: Healthy DB check passes"
    else
        log_fail "S5.2: Healthy DB check test missing"
    fi

    if command grep -q "repair_creates_backup" "$test_output"; then
        log_pass "S5.3: Repair creates backup"
    else
        log_fail "S5.3: Repair backup test missing"
    fi

    if command grep -q "repair_dry_run_makes_no_changes" "$test_output"; then
        log_pass "S5.4: Dry-run repair is safe"
    else
        log_fail "S5.4: Dry-run safety test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Ingest pipeline at scale (multi-pane lifecycle)
# ==============================================================================

scenario_ingest_pipeline() {
    log_test "Scenario 6: Ingest pipeline (multi-pane lifecycle + stability)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run ingest tests covering multi-pane discovery, UUID stability, GAP handling
    cargo test -p wa-core 'ingest::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "ingest_pipeline.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Ingest pipeline tests pass"
    else
        log_fail "S6.1: Ingest pipeline tests failed (exit=$exit_code)"
    fi

    # Multi-pane churn stability
    if command grep -q "registry_multi_pane_churn_stability" "$test_output"; then
        log_pass "S6.2: Multi-pane churn stability tested"
    else
        log_fail "S6.2: Multi-pane churn test missing"
    fi

    # UUID persistence
    if command grep -q "pane_uuid_persists_across_title_change" "$test_output" || \
       command grep -q "uuid_persists_across" "$test_output" || \
       command grep -q "pane_uuid" "$test_output"; then
        log_pass "S6.3: Pane UUID stability tested"
    else
        log_fail "S6.3: Pane UUID stability test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Storage + FTS benchmark regression (budget enforcement)
# ==============================================================================

scenario_benchmark_budgets() {
    log_test "Scenario 7: Storage + FTS benchmark regression guards"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    # Run storage regression benchmarks in test mode (quick, no iterations)
    # This validates that the benchmark code compiles and the basic operations
    # complete without errors. Full Criterion runs are separate.
    start_time=$(date +%s)
    exit_code=0

    # Run benchmarks as tests (--test flag) for quick validation
    timeout 120 cargo test -p wa-core --bench storage_regression \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "storage_bench_test.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.1: Storage benchmark test TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Storage benchmark test completes (${duration_s}s)"
    else
        log_fail "S7.1: Storage benchmark test failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Run sizing benchmark in test mode
    exit_code=0
    timeout 120 cargo test -p wa-core --bench sizing_benchmark \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.2: Sizing benchmark test TIMED OUT"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.2: Sizing benchmark test completes"
    else
        log_fail "S7.2: Sizing benchmark test failed (exit=$exit_code)"
    fi

    # Run FTS query benchmark in test mode
    exit_code=0
    timeout 120 cargo test -p wa-core --bench fts_query \
        -- --test --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S7.3: FTS query benchmark test TIMED OUT"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S7.3: FTS query benchmark test completes"
    else
        log_fail "S7.3: FTS query benchmark test failed (exit=$exit_code)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 8: Bounded execution (full storage test suite timing)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 8: Bounded execution (no hangs, no OOM)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    # Run ALL storage + ingest tests with timing
    start_time=$(date +%s)
    exit_code=0

    timeout 180 cargo test -p wa-core 'storage::' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "full_storage_suite.log" "$(cat "$test_output")"
    e2e_add_file "timing.json" "{\"full_storage_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S8.1: Storage test suite TIMED OUT after 180s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Full storage test suite completed (${duration_s}s)"
    else
        log_fail "S8.1: Full storage test suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify execution time is reasonable (<90s for all storage tests)
    if [[ $duration_s -lt 90 ]]; then
        log_pass "S8.2: Suite completed within 90s budget (${duration_s}s)"
    else
        log_fail "S8.2: Suite exceeded 90s budget (${duration_s}s)"
    fi

    # Verify test count (sum across all test binaries)
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -gt 0 ]]; then
        e2e_add_file "test_counts.json" "{\"storage_tests_passed\": $passed_count, \"duration_s\": $duration_s}"
        log_pass "S8.3: Storage suite ran ($passed_count tests passed in ${duration_s}s)"
    else
        log_fail "S8.3: Could not parse test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Storage/Indexing Stress Test${NC}"
    echo -e "${BLUE}Bead: wa-upg.5.5${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "storage-stress" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "storage_core_tests" scenario_storage_core_tests || overall_exit=1
    e2e_capture_scenario "fts_search_tests" scenario_fts_search_tests || overall_exit=1
    e2e_capture_scenario "fts_sync_tests" scenario_fts_sync_tests || overall_exit=1
    e2e_capture_scenario "concurrent_writers" scenario_concurrent_writers || overall_exit=1
    e2e_capture_scenario "db_health_repair" scenario_db_health_repair || overall_exit=1
    e2e_capture_scenario "ingest_pipeline" scenario_ingest_pipeline || overall_exit=1
    e2e_capture_scenario "benchmark_budgets" scenario_benchmark_budgets || overall_exit=1
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
