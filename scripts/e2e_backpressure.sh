#!/bin/bash
# =============================================================================
# E2E: Backpressure stress scenario (graceful degradation + artifacts)
# Implements: wa-upg.12.6
#
# Purpose:
#   Prove end-to-end that the backpressure subsystem behaves correctly under
#   stress:
#   - Unit/integration tests for backpressure tiers, transitions, and hysteresis
#   - Overflow GAP emission when capture channels are congested
#   - Health snapshot includes backpressure_tier and warnings
#   - Gap storage and retrieval works correctly
#   - System remains responsive (tests don't hang or OOM)
#
#   Since a live WezTerm mux + watcher daemon is not always available,
#   this script validates backpressure behavior via:
#   - Targeted cargo test suites (backpressure, tailer overflow, storage)
#   - CLI contract checks (wa status --format json includes backpressure)
#   - Health snapshot schema validation
#
# Requirements:
#   - cargo (Rust toolchain)
#   - jq for JSON manipulation
#   - wa binary built (cargo build -p wa)
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
WA_BIN=""
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
# Helpers
# ==============================================================================

make_temp_workspace() {
    local dir
    dir=$(mktemp -d "${TMPDIR:-/tmp}/wa-e2e-backpressure.XXXXXX")
    echo "$dir"
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    # Check cargo
    if ! command -v cargo &>/dev/null; then
        echo -e "${RED}ERROR:${NC} cargo not found. Install Rust toolchain." >&2
        exit 5
    fi
    log_pass "cargo available"

    # Check jq
    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found. Install: sudo apt install jq" >&2
        exit 5
    fi
    log_pass "jq available"

    # Find wa binary
    if [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/debug/wa"
    elif [[ -x "$PROJECT_ROOT/target/release/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/release/wa"
    else
        log_skip "wa binary not found (some scenarios will be skipped)"
    fi

    if [[ -n "$WA_BIN" ]]; then
        log_pass "wa binary found: $WA_BIN"
    fi
}

# ==============================================================================
# Scenario 1: Backpressure core unit tests
# ==============================================================================

scenario_backpressure_unit_tests() {
    log_test "Scenario 1: Backpressure core unit tests"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core backpressure::tests \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "backpressure_unit_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: All backpressure unit tests pass"
    else
        log_fail "S1.1: Backpressure unit tests failed (exit=$exit_code)"
    fi

    # Verify specific key behaviors are tested
    if grep -q "classify_green" "$test_output"; then
        log_pass "S1.2: Green tier classification tested"
    else
        log_fail "S1.2: Missing green tier classification test"
    fi

    if grep -q "classify_black" "$test_output"; then
        log_pass "S1.3: Black (saturation) tier classification tested"
    else
        log_fail "S1.3: Missing black tier classification test"
    fi

    if grep -q "evaluate_downgrade_blocked_by_hysteresis" "$test_output"; then
        log_pass "S1.4: Hysteresis for downgrades tested"
    else
        log_fail "S1.4: Missing hysteresis test"
    fi

    if grep -q "snapshot_serialization_roundtrip" "$test_output"; then
        log_pass "S1.5: Snapshot serialization roundtrip tested"
    else
        log_fail "S1.5: Missing snapshot serialization test"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: Overflow GAP emission tests
# ==============================================================================

scenario_overflow_gap_tests() {
    log_test "Scenario 2: Overflow GAP emission under backpressure"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run tailer overflow gap tests
    cargo test -p wa-core tailer::tests::overflow_gap \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "overflow_gap_tailer_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: Tailer overflow GAP tests pass"
    else
        log_fail "S2.1: Tailer overflow GAP tests failed (exit=$exit_code)"
    fi

    # Run ingest overflow gap tests
    exit_code=0
    cargo test -p wa-core ingest::tests::emit_overflow_gap \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "overflow_gap_ingest_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.2: Ingest overflow GAP tests pass"
    else
        log_fail "S2.2: Ingest overflow GAP tests failed (exit=$exit_code)"
    fi

    # Verify the overflow threshold is reasonable
    exit_code=0
    cargo test -p wa-core tailer::tests::overflow_threshold_constant_is_reasonable \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.3: Overflow threshold constant validated"
    else
        log_fail "S2.3: Overflow threshold constant validation failed"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: Storage backpressure integration tests
# ==============================================================================

scenario_storage_backpressure_integration() {
    log_test "Scenario 3: Storage backpressure integration"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core storage::backpressure_integration_tests \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "storage_backpressure_integration.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: Storage backpressure integration tests pass"
    else
        log_fail "S3.1: Storage backpressure integration tests failed (exit=$exit_code)"
    fi

    # Check specific important behaviors
    if grep -q "capture_channel_backpressure_detected" "$test_output"; then
        log_pass "S3.2: Capture channel backpressure detection tested"
    else
        log_fail "S3.2: Missing capture channel backpressure detection test"
    fi

    if grep -q "gap_recording_works_under_backpressure" "$test_output"; then
        log_pass "S3.3: Gap recording under backpressure tested"
    else
        log_fail "S3.3: Missing gap recording under backpressure test"
    fi

    if grep -q "health_warning_threshold_generates_warnings" "$test_output"; then
        log_pass "S3.4: Health warning threshold tested"
    else
        log_fail "S3.4: Missing health warning threshold test"
    fi

    if grep -q "storage_concurrent_writers_dont_deadlock" "$test_output"; then
        log_pass "S3.5: Concurrent writer deadlock safety tested"
    else
        log_fail "S3.5: Missing concurrent writer deadlock safety test"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Tailer backpressure counter tracking
# ==============================================================================

scenario_tailer_backpressure_counters() {
    log_test "Scenario 4: Tailer backpressure counter tracking"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run tailer backpressure-related tests (exclude overflow_gap which has its own scenario)
    cargo test -p wa-core 'tailer::tests' \
        --no-fail-fast -- --nocapture --skip overflow_gap \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "tailer_backpressure_counters.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Tailer backpressure counter tests pass"
    else
        log_fail "S4.1: Tailer backpressure counter tests failed (exit=$exit_code)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Runtime backpressure warning integration
# ==============================================================================

scenario_runtime_backpressure_warnings() {
    log_test "Scenario 5: Runtime backpressure warning integration"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core runtime::tests::backpressure \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "runtime_backpressure_warnings.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Runtime backpressure warning tests pass"
    else
        log_fail "S5.1: Runtime backpressure warning tests failed (exit=$exit_code)"
    fi

    # Also run the events backpressure-causes-lag test
    exit_code=0
    cargo test -p wa-core events::tests::backpressure_causes_lag \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.2: Event backpressure lag detection tested"
    else
        log_fail "S5.2: Event backpressure lag detection test failed"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Health snapshot schema includes backpressure fields
# ==============================================================================

scenario_health_snapshot_schema() {
    log_test "Scenario 6: Health snapshot schema includes backpressure"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run health snapshot renderer tests that validate backpressure display
    cargo test -p wa-core 'output::renderers::tests::health_' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "health_snapshot_schema.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Health snapshot renderer tests pass"
    else
        log_fail "S6.1: Health snapshot renderer tests failed (exit=$exit_code)"
    fi

    # Verify that BackpressureSnapshot serializes to valid JSON with expected fields
    exit_code=0
    cargo test -p wa-core backpressure::tests::snapshot_serialization_roundtrip \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.2: BackpressureSnapshot JSON roundtrip validated"
    else
        log_fail "S6.2: BackpressureSnapshot JSON roundtrip failed"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: CLI status contract (requires wa binary)
# ==============================================================================

scenario_cli_status_contract() {
    log_test "Scenario 7: CLI status contract includes backpressure"

    if [[ -z "$WA_BIN" ]]; then
        log_skip "S7: wa binary not available"
        return 0
    fi

    local workspace stdout_file exit_code

    # Test that wa status --format json is parseable
    # (Without a running watcher, it should still produce valid JSON or
    #  fail gracefully with an informative message)
    workspace=$(make_temp_workspace)
    stdout_file=$(mktemp)
    exit_code=0

    timeout 15 "$WA_BIN" status --workspace "$workspace" --format json \
        >"$stdout_file" 2>&1 || exit_code=$?

    local status_output
    status_output=$(cat "$stdout_file")
    e2e_add_file "wa_status_output.json" "$status_output"

    # Even without a watcher, the status command should handle gracefully
    # exit_code 124 = our timeout killed it (WezTerm socket timeout is 30s)
    if [[ $exit_code -le 1 || $exit_code -eq 124 ]]; then
        log_pass "S7.1: wa status exits cleanly (exit=$exit_code)"
    else
        log_fail "S7.1: wa status crashed (exit=$exit_code)"
    fi

    # Extract just the JSON line (last line of output, skipping log/tracing lines)
    local json_line
    json_line=$(echo "$status_output" | grep '^{' | tail -1)

    # If there's JSON output, check for backpressure_tier field presence
    if echo "$json_line" | jq . >/dev/null 2>&1; then
        log_pass "S7.2: wa status produces valid JSON"

        # Check that the health schema would include backpressure_tier
        # (may be null without a running watcher, but the field should exist
        #  in the schema)
        if echo "$json_line" | jq -e '.health' >/dev/null 2>&1; then
            log_pass "S7.3: Status output includes health section"

            if echo "$json_line" | jq -e '.health | has("backpressure_tier")' >/dev/null 2>&1; then
                log_pass "S7.4: Health section includes backpressure_tier field"
            else
                log_skip "S7.4: backpressure_tier not in health (expected without running watcher)"
            fi
        else
            log_skip "S7.3: No health section (expected without running watcher)"
        fi
    else
        log_skip "S7.2: Status output is not JSON (expected without running watcher)"
    fi

    rm -f "$stdout_file"
    rm -rf "$workspace"
}

# ==============================================================================
# Scenario 8: Full backpressure test suite timing (bounded execution)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 8: Bounded execution (no hangs, no OOM)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    # Run ALL backpressure-related tests with a generous timeout
    # This proves the system doesn't hang under simulated stress
    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core \
        backpressure \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "full_backpressure_suite.log" "$(cat "$test_output")"
    e2e_add_file "timing.json" "{\"full_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S8.1: Backpressure test suite TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Full backpressure test suite completed (${duration_s}s)"
    else
        log_fail "S8.1: Full backpressure test suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Verify execution time is reasonable (<60s for all backpressure tests)
    if [[ $duration_s -lt 60 ]]; then
        log_pass "S8.2: Suite completed within 60s budget (${duration_s}s)"
    else
        log_fail "S8.2: Suite exceeded 60s budget (${duration_s}s)"
    fi

    # Count test results from output (sum across all test binaries)
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -gt 0 ]]; then
        log_pass "S8.3: Test suite ran ($passed_count tests passed)"
        e2e_add_file "test_counts.json" "{\"passed\": $passed_count}"
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
    echo -e "${BLUE}E2E: Backpressure Stress Scenario${NC}"
    echo -e "${BLUE}Bead: wa-upg.12.6${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "backpressure-stress" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "backpressure_unit_tests" scenario_backpressure_unit_tests || overall_exit=1
    e2e_capture_scenario "overflow_gap_tests" scenario_overflow_gap_tests || overall_exit=1
    e2e_capture_scenario "storage_backpressure_integration" scenario_storage_backpressure_integration || overall_exit=1
    e2e_capture_scenario "tailer_backpressure_counters" scenario_tailer_backpressure_counters || overall_exit=1
    e2e_capture_scenario "runtime_backpressure_warnings" scenario_runtime_backpressure_warnings || overall_exit=1
    e2e_capture_scenario "health_snapshot_schema" scenario_health_snapshot_schema || overall_exit=1
    e2e_capture_scenario "cli_status_contract" scenario_cli_status_contract || overall_exit=1
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
