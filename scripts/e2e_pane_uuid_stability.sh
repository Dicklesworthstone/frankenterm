#!/bin/bash
# =============================================================================
# E2E: pane_uuid remains stable across rename/move
# Implements: wa-upg.4.6
#
# Purpose:
#   Prove end-to-end that pane_uuid identity is stable across common churn:
#   - UUID assigned on pane discovery
#   - UUID stable across title rename, tab move, cwd change
#   - UUID regenerated only on genuine reappearance (new session)
#   - Fingerprint-based generation detection works correctly
#   - Multi-pane churn (open/close/reopen) doesn't corrupt identity
#   - Pane records persist correct UUID through storage roundtrip
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
# Scenario 1: UUID assignment and format
# ==============================================================================

scenario_uuid_assignment() {
    log_test "Scenario 1: UUID assignment and format"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run UUID format + assignment tests (pane_uuid matches both format and entropy tests)
    cargo test -p wa-core 'pane_uuid' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "uuid_assignment.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: UUID format and assignment tests pass"
    else
        log_fail "S1.1: UUID tests failed (exit=$exit_code)"
    fi

    if command grep -q "pane_uuid_format_is_32_hex_chars" "$test_output"; then
        log_pass "S1.2: UUID format validated (32 hex chars)"
    else
        log_fail "S1.2: UUID format test missing"
    fi

    if command grep -q "pane_uuid_includes_random_entropy" "$test_output"; then
        log_pass "S1.3: UUID includes entropy"
    else
        log_fail "S1.3: UUID entropy test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: UUID stability across metadata changes
# ==============================================================================

scenario_uuid_stability_metadata() {
    log_test "Scenario 2: UUID stability across metadata changes"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run all registry_uuid_stable tests
    cargo test -p wa-core 'registry_uuid_stable' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "uuid_stability_metadata.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: UUID stability tests pass"
    else
        log_fail "S2.1: UUID stability tests failed (exit=$exit_code)"
    fi

    # Key stability behaviors
    if command grep -q "registry_uuid_stable_across_title_change" "$test_output"; then
        log_pass "S2.2: UUID stable across title rename"
    else
        log_fail "S2.2: Title rename stability test missing"
    fi

    if command grep -q "registry_uuid_stable_across_tab_move" "$test_output"; then
        log_pass "S2.3: UUID stable across tab move"
    else
        log_fail "S2.3: Tab move stability test missing"
    fi

    if command grep -q "registry_uuid_stable_across_cwd_change" "$test_output"; then
        log_pass "S2.4: UUID stable across cwd change"
    else
        log_fail "S2.4: CWD change stability test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: Multi-pane churn stability
# ==============================================================================

scenario_multi_pane_churn() {
    log_test "Scenario 3: Multi-pane churn stability"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'registry_multi_pane_churn_stability' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "multi_pane_churn.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: Multi-pane churn stability test passes"
    else
        log_fail "S3.1: Multi-pane churn stability failed (exit=$exit_code)"
    fi

    # Test pane close/reappearance handling
    exit_code=0
    cargo test -p wa-core 'registry_new_uuid_on_reappearance' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.2: New UUID on genuine reappearance"
    else
        log_fail "S3.2: Reappearance UUID test failed"
    fi

    # UUID removed on close
    exit_code=0
    cargo test -p wa-core 'registry_uuid_removed_on_close' \
        -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.3: UUID cleaned up on pane close"
    else
        log_fail "S3.3: UUID close cleanup test failed"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Fingerprint-based generation detection
# ==============================================================================

scenario_fingerprint_generation() {
    log_test "Scenario 4: Fingerprint-based generation detection"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'ingest::tests::fingerprint' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "fingerprint_generation.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Fingerprint generation tests pass"
    else
        log_fail "S4.1: Fingerprint tests failed (exit=$exit_code)"
    fi

    # Key fingerprint behaviors
    if command grep -q "fingerprint_same_generation_when_unchanged" "$test_output"; then
        log_pass "S4.2: Same generation when pane unchanged"
    else
        log_fail "S4.2: Unchanged generation test missing"
    fi

    if command grep -q "fingerprint_ignores_tab_window_change" "$test_output"; then
        log_pass "S4.3: Tab/window changes don't affect fingerprint"
    else
        log_fail "S4.3: Tab/window fingerprint test missing"
    fi

    if command grep -q "fingerprint_new_generation_on_title_change" "$test_output"; then
        log_pass "S4.4: New generation on title change detected"
    else
        log_fail "S4.4: Title change generation test missing"
    fi

    if command grep -q "fingerprint_new_generation_on_domain_change" "$test_output"; then
        log_pass "S4.5: New generation on domain change detected"
    else
        log_fail "S4.5: Domain change generation test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Registry lifecycle (full discovery flow)
# ==============================================================================

scenario_registry_lifecycle() {
    log_test "Scenario 5: Registry lifecycle (discovery + tracking)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run all registry tests
    cargo test -p wa-core 'ingest::tests::registry' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "registry_lifecycle.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Registry lifecycle tests pass"
    else
        log_fail "S5.1: Registry lifecycle tests failed (exit=$exit_code)"
    fi

    # Key lifecycle behaviors
    if command grep -q "registry_tracks_panes" "$test_output"; then
        log_pass "S5.2: Pane tracking works"
    else
        log_fail "S5.2: Pane tracking test missing"
    fi

    if command grep -q "registry_uuid_reverse_index_consistent" "$test_output"; then
        log_pass "S5.3: UUID reverse index consistent"
    else
        log_fail "S5.3: Reverse index consistency test missing"
    fi

    if command grep -q "registry_to_pane_records" "$test_output"; then
        log_pass "S5.4: Registry converts to storage PaneRecords"
    else
        log_fail "S5.4: PaneRecord conversion test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Discovery tick (pane lifecycle events)
# ==============================================================================

scenario_discovery_tick() {
    log_test "Scenario 6: Discovery tick (pane lifecycle events)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'ingest::tests::discovery_tick' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "discovery_tick.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Discovery tick tests pass"
    else
        log_fail "S6.1: Discovery tick tests failed (exit=$exit_code)"
    fi

    if command grep -q "discovery_tick_detects_new_panes" "$test_output"; then
        log_pass "S6.2: New pane detection works"
    else
        log_fail "S6.2: New pane detection test missing"
    fi

    if command grep -q "discovery_tick_detects_closed_panes" "$test_output"; then
        log_pass "S6.3: Closed pane detection works"
    else
        log_fail "S6.3: Closed pane detection test missing"
    fi

    if command grep -q "discovery_tick_detects_metadata_changes" "$test_output"; then
        log_pass "S6.4: Metadata change detection works"
    else
        log_fail "S6.4: Metadata change detection test missing"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Pane record storage persistence
# ==============================================================================

scenario_pane_storage_persistence() {
    log_test "Scenario 7: Pane record storage persistence"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run pane entry conversion and persistence tests
    cargo test -p wa-core 'ingest::tests::pane_entry' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "pane_storage_persistence.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Pane entry/record tests pass"
    else
        log_fail "S7.1: Pane entry tests failed (exit=$exit_code)"
    fi

    # Also check persist_captured tests
    exit_code=0
    cargo test -p wa-core 'ingest::tests::persist_captured' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S7.2: Segment persistence tests pass"
    else
        log_fail "S7.2: Segment persistence failed (exit=$exit_code)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 8: Bounded execution (full ingest test suite timing)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 8: Bounded execution (full ingest suite timing)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 60 cargo test -p wa-core 'ingest::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "full_ingest_suite.log" "$(cat "$test_output")"
    e2e_add_file "timing.json" "{\"ingest_suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S8.1: Ingest suite TIMED OUT after 60s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Full ingest suite completed (${duration_s}s)"
    else
        log_fail "S8.1: Full ingest suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Should complete well within 30s
    if [[ $duration_s -lt 30 ]]; then
        log_pass "S8.2: Ingest suite within 30s budget (${duration_s}s)"
    else
        log_fail "S8.2: Ingest suite exceeded 30s budget (${duration_s}s)"
    fi

    # Count tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 50 ]]; then
        log_pass "S8.3: Ingest test count >= 50 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S8.3: Fewer ingest tests than expected ($passed_count, expected >= 50)"
    else
        log_fail "S8.3: Could not parse ingest test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Pane UUID Stability${NC}"
    echo -e "${BLUE}Bead: wa-upg.4.6${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "pane-uuid-stability" >/dev/null

    local overall_exit=0

    # Run all scenarios
    e2e_capture_scenario "uuid_assignment" scenario_uuid_assignment || overall_exit=1
    e2e_capture_scenario "uuid_stability_metadata" scenario_uuid_stability_metadata || overall_exit=1
    e2e_capture_scenario "multi_pane_churn" scenario_multi_pane_churn || overall_exit=1
    e2e_capture_scenario "fingerprint_generation" scenario_fingerprint_generation || overall_exit=1
    e2e_capture_scenario "registry_lifecycle" scenario_registry_lifecycle || overall_exit=1
    e2e_capture_scenario "discovery_tick" scenario_discovery_tick || overall_exit=1
    e2e_capture_scenario "pane_storage_persistence" scenario_pane_storage_persistence || overall_exit=1
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
