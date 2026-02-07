#!/bin/bash
# =============================================================================
# E2E: Incident bundle export + replay (verbose logs + artifacts)
# Implements: wa-upg.1.6
#
# Purpose:
#   Validate the full incident bundle lifecycle:
#   - Bundle format, manifest, and privacy budget contracts
#   - Crash bundle creation with secret redaction (all patterns)
#   - Enhanced collector: DB metadata, recent events, config redaction
#   - Replay validation: policy mode and rules mode checks
#   - Deterministic replay detects secret leaks and invalid bundles
#   - Edge cases: unicode, empty messages, large configs, size budgets
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
# Scenario 1: Bundle format and manifest contracts
# ==============================================================================

scenario_bundle_format_contracts() {
    log_test "Scenario 1: Bundle format and manifest contracts"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'incident_bundle::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "bundle_format_contracts.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S1.1: Bundle format contract tests pass"
    else
        log_fail "S1.1: Bundle format tests failed (exit=$exit_code)"
    fi

    # Verify format version, privacy budget, and replay mode contracts
    for test_name in format_version_is_compatible_with default_budget_is_valid bundle_dirname_format bundle_file_filenames_are_unique; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S1.${TESTS_RUN}: $test_name tested"
        else
            log_fail "S1.${TESTS_RUN}: $test_name test missing"
        fi
    done

    # Verify replay mode invariants
    for test_name in all_replay_modes_have_manifest_check all_replay_modes_have_secret_leak_check all_replay_modes_have_version_check; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S1.${TESTS_RUN}: $test_name tested"
        else
            log_fail "S1.${TESTS_RUN}: $test_name test missing"
        fi
    done

    # Count format tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 30 ]]; then
        log_pass "S1.${TESTS_RUN}: $passed_count bundle format tests validated (>= 30)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S1.${TESTS_RUN}: Only $passed_count format tests (expected >= 30)"
    else
        log_fail "S1.${TESTS_RUN}: Could not parse format test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 2: Crash bundle redaction (all secret patterns)
# ==============================================================================

scenario_crash_bundle_redaction() {
    log_test "Scenario 2: Crash bundle secret redaction (all patterns)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core --test incident_bundle_tests 'crash_bundle_redacts' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "crash_bundle_redaction.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S2.1: Crash bundle redaction tests pass"
    else
        log_fail "S2.1: Crash bundle redaction failed (exit=$exit_code)"
    fi

    # Verify each secret pattern has a dedicated test
    for pattern in anthropic_key openai_key github_token bearer_token database_url stripe_key aws_access_key multiple_secrets; do
        if command grep -q "crash_bundle_redacts_$pattern" "$test_output"; then
            log_pass "S2.${TESTS_RUN}: $pattern redaction tested"
        else
            log_fail "S2.${TESTS_RUN}: $pattern redaction missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 3: Enhanced incident bundle collector
# ==============================================================================

scenario_incident_bundle_collector() {
    log_test "Scenario 3: Enhanced incident bundle collector"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core --test incident_bundle_tests 'collect_incident_bundle' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "incident_bundle_collector.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S3.1: Incident bundle collector tests pass"
    else
        log_fail "S3.1: Incident bundle collector failed (exit=$exit_code)"
    fi

    # Verify collector features
    for test_name in manual_kind_produces_manifest with_db_metadata includes_recent_events max_events_limits_output with_config_redacts_secrets crash_kind_includes_crash_data files_list_matches_disk; do
        if command grep -q "collect_incident_bundle_$test_name" "$test_output"; then
            log_pass "S3.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S3.${TESTS_RUN}: $test_name test missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 4: Replay validation (policy + rules modes)
# ==============================================================================

scenario_replay_validation() {
    log_test "Scenario 4: Replay validation (policy + rules modes)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core --test incident_bundle_tests 'replay' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "replay_validation.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S4.1: Replay validation tests pass"
    else
        log_fail "S4.1: Replay validation failed (exit=$exit_code)"
    fi

    # Verify both replay modes
    for test_name in replay_clean_bundle_policy_mode_passes replay_bundle_rules_mode_validates_events replay_detects_secret_leak_in_bundle replay_empty_bundle_dir_fails_manifest replay_crash_kind_bundle_validates_crash_report; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S4.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S4.${TESTS_RUN}: $test_name test missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 5: Crash module unit tests (panic hook, manifest, timestamps)
# ==============================================================================

scenario_crash_module_tests() {
    log_test "Scenario 5: Crash module unit tests"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'crash::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "crash_module_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Crash module unit tests pass"
    else
        log_fail "S5.1: Crash module tests failed (exit=$exit_code)"
    fi

    # Verify key crash module behaviors
    for test_name in crash_manifest_serialization crash_report_serialization export_incident_bundle_crash_with_bundle export_incident_bundle_manual_kind write_crash_bundle_redacts_secrets; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S5.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S5.${TESTS_RUN}: $test_name test missing"
        fi
    done

    # Count crash module tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 25 ]]; then
        log_pass "S5.${TESTS_RUN}: $passed_count crash module tests validated (>= 25)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S5.${TESTS_RUN}: Only $passed_count crash tests (expected >= 25)"
    else
        log_fail "S5.${TESTS_RUN}: Could not parse crash test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: Redaction standalone tests (patterns, edge cases)
# ==============================================================================

scenario_redaction_standalone() {
    log_test "Scenario 6: Redaction standalone tests"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'redact' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "redaction_standalone.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S6.1: Redaction tests pass"
    else
        log_fail "S6.1: Redaction tests failed (exit=$exit_code)"
    fi

    # Count redaction tests across all modules
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 15 ]]; then
        log_pass "S6.2: $passed_count redaction tests across all modules (>= 15)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S6.2: Only $passed_count redaction tests (expected >= 15)"
    else
        log_fail "S6.2: Could not parse redaction test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 7: Replay module tests (frame decoding, asciinema export)
# ==============================================================================

scenario_replay_module() {
    log_test "Scenario 7: Replay module tests (frame decoding + asciinema)"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    cargo test -p wa-core 'replay::tests' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "replay_module.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S7.1: Replay module tests pass"
    else
        log_fail "S7.1: Replay module tests failed (exit=$exit_code)"
    fi

    # Verify frame decoding and asciinema export
    for test_name in decode_output_frame decode_input_frame decode_event_frame export_asciinema_header_and_events export_asciinema_redaction; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S7.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S7.${TESTS_RUN}: $test_name test missing"
        fi
    done

    # Count replay tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 15 ]]; then
        log_pass "S7.${TESTS_RUN}: $passed_count replay module tests (>= 15)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S7.${TESTS_RUN}: Only $passed_count replay tests (expected >= 15)"
    else
        log_fail "S7.${TESTS_RUN}: Could not parse replay test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 8: Edge cases and size budgets
# ==============================================================================

scenario_edge_cases() {
    log_test "Scenario 8: Edge cases and size budgets"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Run full integration test suite and validate edge cases are present
    cargo test -p wa-core --test incident_bundle_tests \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "edge_cases.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S8.1: Edge case tests pass"
    else
        log_fail "S8.1: Edge case tests failed (exit=$exit_code)"
    fi

    # Verify specific edge cases
    for test_name in crash_bundle_with_unicode_message crash_bundle_with_empty_message crash_bundle_enforces_size_budget incident_export_with_large_config_truncates multiple_crash_bundles_have_unique_names; do
        if command grep -q "$test_name" "$test_output"; then
            log_pass "S8.${TESTS_RUN}: $test_name validated"
        else
            log_fail "S8.${TESTS_RUN}: $test_name test missing"
        fi
    done

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 9: Bounded execution (full incident bundle test suite timing)
# ==============================================================================

scenario_bounded_execution() {
    log_test "Scenario 9: Bounded execution (incident bundle suite timing)"

    local test_output exit_code start_time end_time duration_s
    test_output=$(mktemp)

    start_time=$(date +%s)
    exit_code=0

    timeout 120 cargo test -p wa-core --test incident_bundle_tests \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    end_time=$(date +%s)
    duration_s=$((end_time - start_time))

    e2e_add_file "incident_bundle_suite_timing.log" "$(cat "$test_output")"
    e2e_add_file "incident_bundle_timing.json" "{\"suite_seconds\": $duration_s, \"exit_code\": $exit_code}"

    if [[ $exit_code -eq 124 ]]; then
        log_fail "S9.1: Incident bundle suite TIMED OUT after 120s"
    elif [[ $exit_code -eq 0 ]]; then
        log_pass "S9.1: Incident bundle suite completed (${duration_s}s)"
    else
        log_fail "S9.1: Incident bundle suite failed (exit=$exit_code, ${duration_s}s)"
    fi

    # Suite should complete within 60s
    if [[ $duration_s -lt 60 ]]; then
        log_pass "S9.2: Suite within 60s budget (${duration_s}s)"
    else
        log_fail "S9.2: Suite exceeded 60s budget (${duration_s}s)"
    fi

    # Count total integration tests
    local passed_count=0
    while IFS= read -r line; do
        local n
        n=$(echo "$line" | command grep -oP '\d+ passed' | command grep -oP '\d+' || echo "0")
        passed_count=$((passed_count + n))
    done < <(command grep 'test result:' "$test_output" 2>/dev/null)

    if [[ "$passed_count" -ge 40 ]]; then
        log_pass "S9.3: Integration test count >= 40 ($passed_count passed)"
    elif [[ "$passed_count" -gt 0 ]]; then
        log_fail "S9.3: Fewer integration tests than expected ($passed_count, expected >= 40)"
    else
        log_fail "S9.3: Could not parse integration test results"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Incident Bundle Export + Replay${NC}"
    echo -e "${BLUE}Bead: wa-upg.1.6${NC}"
    echo -e "${BLUE}================================================${NC}"

    check_prerequisites

    e2e_init_artifacts "incident-bundle" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "bundle_format_contracts" scenario_bundle_format_contracts || overall_exit=1
    e2e_capture_scenario "crash_bundle_redaction" scenario_crash_bundle_redaction || overall_exit=1
    e2e_capture_scenario "incident_bundle_collector" scenario_incident_bundle_collector || overall_exit=1
    e2e_capture_scenario "replay_validation" scenario_replay_validation || overall_exit=1
    e2e_capture_scenario "crash_module_tests" scenario_crash_module_tests || overall_exit=1
    e2e_capture_scenario "redaction_standalone" scenario_redaction_standalone || overall_exit=1
    e2e_capture_scenario "replay_module" scenario_replay_module || overall_exit=1
    e2e_capture_scenario "edge_cases" scenario_edge_cases || overall_exit=1
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
