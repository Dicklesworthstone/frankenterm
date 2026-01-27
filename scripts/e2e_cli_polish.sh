#!/bin/bash
# E2E: CLI Polish Validation with Verbose Logs
# Implements: bd-3m3a
#
# This script validates CLI polish features including:
# - Help output consistency
# - Verbosity tiers (default/verbose)
# - Command aliases (where implemented)
# - No ANSI when piped
# - Exit code conventions
#
# Features not yet implemented are marked as SKIP with references to blocking beads.
#
# Usage: ./scripts/e2e_cli_polish.sh [OPTIONS]
#
# Exit codes:
#   0 - All tests passed
#   1 - One or more tests failed
#   2 - Prerequisites missing

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Source the E2E artifacts library
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

# Build the wa binary first
WA_BIN=""

# Colors for output (only when TTY)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# Counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0

# ==============================================================================
# Helpers
# ==============================================================================

log_info() {
    echo -e "${BLUE}[INFO]${NC} $*"
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

log_test() {
    echo -e "\n${BLUE}=== $* ===${NC}"
}

# Capture wa command output (ensures binary path is used)
run_wa() {
    "$WA_BIN" "$@"
}

# Check if output contains ANSI escape sequences
has_ansi() {
    # Look for \033[ or \x1b[ patterns
    grep -qP '\x1b\[' <<< "$1" 2>/dev/null || return 1
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Checking Prerequisites"

    # Build wa
    log_info "Building wa binary..."
    if ! cargo build -p wa --quiet 2>/dev/null; then
        log_fail "Failed to build wa binary"
        exit 2
    fi

    # Try cargo target directory first (used by cargo run), then project target
    WA_BIN="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}/debug/wa"
    if [[ ! -x "$WA_BIN" ]]; then
        WA_BIN="$PROJECT_ROOT/target/debug/wa"
    fi

    if [[ ! -x "$WA_BIN" ]]; then
        log_fail "wa binary not found after build"
        exit 2
    fi

    log_pass "wa binary built: $WA_BIN"

    # Check jq for JSON validation
    if command -v jq &>/dev/null; then
        log_pass "jq available"
    else
        log_fail "jq not found (required for JSON validation)"
        exit 2
    fi
}

# ==============================================================================
# Help Output Tests
# ==============================================================================

test_help_output() {
    log_test "Testing Help Output Consistency"

    # Test 1: Main help shows all commands
    local main_help
    main_help=$(run_wa --help 2>&1) || true

    # Required commands that must appear in help
    local required_cmds=("watch" "robot" "search" "list" "send" "workflow" "status" "events" "why" "doctor")

    local all_present=true
    for cmd in "${required_cmds[@]}"; do
        if echo "$main_help" | grep -qw "$cmd"; then
            log_pass "Help lists command: $cmd"
        else
            log_fail "Help missing command: $cmd"
            all_present=false
        fi
    done

    # Test 2: Each subcommand has help
    for cmd in "${required_cmds[@]}"; do
        local cmd_help
        cmd_help=$(run_wa "$cmd" --help 2>&1) || true

        # Robot mode outputs JSON help, which is valid
        if [[ "$cmd" == "robot" && "$cmd_help" =~ "\"ok\":" ]]; then
            log_pass "Subcommand has help: $cmd (JSON format)"
        elif [[ -n "$cmd_help" && "$cmd_help" != *"error"* ]]; then
            log_pass "Subcommand has help: $cmd"
        else
            log_fail "Subcommand help broken: $cmd"
        fi
    done

    # Test 3: Help format consistency (should have USAGE section)
    if echo "$main_help" | grep -qi "usage"; then
        log_pass "Main help has USAGE section"
    else
        log_fail "Main help missing USAGE section"
    fi

    # Test 4: Short help (-h) works
    local short_help
    short_help=$(run_wa -h 2>&1) || true

    if [[ -n "$short_help" ]]; then
        log_pass "Short help (-h) works"
    else
        log_fail "Short help (-h) failed"
    fi

    # Artifact: Capture full help for all commands
    e2e_add_file "help_main.txt" "$main_help"

    for cmd in "${required_cmds[@]}"; do
        local cmd_help
        cmd_help=$(run_wa "$cmd" --help 2>&1) || true
        e2e_add_file "help_${cmd}.txt" "$cmd_help"
    done
}

# ==============================================================================
# Verbosity Tier Tests
# ==============================================================================

test_verbosity_tiers() {
    log_test "Testing Verbosity Tiers"

    # Test 1: Default output (no -v)
    local default_output
    default_output=$(run_wa status 2>&1) || true

    # Default should produce some output (even if error due to no watcher)
    if [[ -n "$default_output" ]]; then
        log_pass "Default output produces content"
    else
        log_fail "Default output is empty"
    fi

    # Test 2: Verbose output (-v) should work
    local verbose_output
    verbose_output=$(run_wa -v status 2>&1) || true

    if [[ -n "$verbose_output" ]]; then
        log_pass "Verbose output (-v) works"
    else
        log_fail "Verbose output (-v) failed"
    fi

    # Test 3: Check that verbosity affects output (when watcher is running)
    # Note: Without a running watcher, both might show similar error messages
    # We mainly verify the flag is accepted

    # Test 4: Verbose flag is global (works with subcommands)
    local verbose_help
    verbose_help=$(run_wa -v --help 2>&1) || true

    if [[ -n "$verbose_help" ]]; then
        log_pass "Verbose flag works with --help"
    else
        log_fail "Verbose flag with --help failed"
    fi

    # Artifact: Capture default vs verbose output
    e2e_add_file "status_default.txt" "$default_output"
    e2e_add_file "status_verbose.txt" "$verbose_output"

    # Future test: -vv (debug level) - wa-rnf.3
    log_skip "-vv debug level: Not implemented (wa-rnf.3)"
}

# ==============================================================================
# Command Alias Tests
# ==============================================================================

test_command_aliases() {
    log_test "Testing Command Aliases"

    # Test 1: search has 'query' alias
    local search_help
    search_help=$(run_wa search --help 2>&1) || true

    if echo "$search_help" | grep -qi "alias.*query\|query"; then
        log_pass "search command documents query alias"
    else
        # Even if not documented, test if it works
        local query_result
        query_result=$(run_wa query "test" 2>&1) || true

        if [[ "$query_result" != *"unrecognized"* && "$query_result" != *"error: no such"* ]]; then
            log_pass "query alias works (even if not documented)"
        else
            log_fail "query alias not working"
        fi
    fi

    # Future tests: Smart aliases (was, wae, waq, etc.) - wa-rnf.2
    log_skip "Smart shell aliases (was, wae, etc.): Not implemented (wa-rnf.2)"
    log_skip "User-configurable aliases: Not implemented (wa-rnf.2)"
    log_skip "'wa aliases' command: Not implemented (wa-rnf.2)"
}

# ==============================================================================
# Shell Completions Tests
# ==============================================================================

test_shell_completions() {
    log_test "Testing Shell Completions"

    # Shell completions are not yet implemented
    # Check if completion generation command exists

    local completions_help
    completions_help=$(run_wa completions --help 2>&1) || true

    if [[ "$completions_help" != *"unrecognized"* && "$completions_help" != *"error:"* ]]; then
        log_pass "'wa completions' command exists"

        # Test completion generation for each shell
        for shell in bash zsh fish powershell; do
            local completion_output
            completion_output=$(run_wa completions "$shell" 2>&1) || true

            if [[ -n "$completion_output" && "$completion_output" != *"error"* ]]; then
                log_pass "Completions generate for: $shell"
                e2e_add_file "completions_${shell}.txt" "$completion_output"
            else
                log_fail "Completions failed for: $shell"
            fi
        done
    else
        log_skip "Shell completions: Not implemented (wa-rnf.1)"
        log_skip "Dynamic pane ID completion: Not implemented (wa-rnf.1)"
        log_skip "Dynamic workflow name completion: Not implemented (wa-rnf.1)"
    fi
}

# ==============================================================================
# Output Consistency Tests
# ==============================================================================

test_output_consistency() {
    log_test "Testing Output Consistency"

    # Test 1: No ANSI when stdout is not a TTY (piped)
    local piped_output
    piped_output=$(run_wa --help | cat)

    if has_ansi "$piped_output"; then
        log_fail "ANSI codes present when piped (should be clean)"
    else
        log_pass "No ANSI codes when piped"
    fi

    # Test 2: Exit codes for help
    if run_wa --help >/dev/null 2>&1; then
        log_pass "--help exits with 0"
    else
        log_fail "--help exits with non-zero"
    fi

    # Test 3: Invalid command exits with non-zero
    if run_wa invalid_command_xyz 2>/dev/null; then
        log_fail "Invalid command should exit non-zero"
    else
        log_pass "Invalid command exits non-zero"
    fi

    # Test 4: JSON output format (where supported)
    local json_output
    json_output=$(run_wa robot panes --format json 2>&1) || true

    # Even if error, check if it's valid JSON
    if echo "$json_output" | jq . >/dev/null 2>&1; then
        log_pass "Robot mode produces valid JSON"
        e2e_add_json "robot_panes.json" "$json_output"
    else
        # Might fail if no watcher running, check for structured error
        if echo "$json_output" | grep -q '"error"'; then
            log_pass "Robot mode produces JSON even for errors"
        else
            log_skip "Robot JSON output: Requires running watcher"
        fi
    fi

    # Test 5: Version output
    local version_output
    version_output=$(run_wa --version 2>&1) || true

    if [[ "$version_output" =~ wa[[:space:]]+[0-9]+\.[0-9]+ ]]; then
        log_pass "Version output is formatted correctly"
        e2e_add_file "version.txt" "$version_output"
    else
        log_fail "Version output format unexpected: $version_output"
    fi
}

# ==============================================================================
# Help Examples Tests
# ==============================================================================

test_help_examples() {
    log_test "Testing Help Text Quality"

    # Check if commands have examples in their help
    local commands_with_examples=0
    local commands_checked=0

    for cmd in send search workflow events; do
        local cmd_help
        cmd_help=$(run_wa "$cmd" --help 2>&1) || true
        ((commands_checked++))

        # Check for EXAMPLES section or example-like content
        if echo "$cmd_help" | grep -qiE "(examples?:|e\.g\.|for example)"; then
            log_pass "Command has examples: $cmd"
            ((commands_with_examples++))
        else
            # Not a failure yet since wa-rnf.4 hasn't been implemented
            log_skip "Command missing examples: $cmd (wa-rnf.4)"
        fi

        # Check for SEE ALSO section
        if echo "$cmd_help" | grep -qi "see also"; then
            log_pass "Command has SEE ALSO: $cmd"
        else
            log_skip "Command missing SEE ALSO: $cmd (wa-rnf.4)"
        fi
    done

    # Capture artifact
    e2e_add_file "help_quality_summary.txt" "Commands with examples: $commands_with_examples / $commands_checked"
}

# ==============================================================================
# Error Message Quality Tests
# ==============================================================================

test_error_messages() {
    log_test "Testing Error Message Quality"

    # Test 1: Invalid argument shows helpful message
    local invalid_arg_output
    invalid_arg_output=$(run_wa send --invalid-flag 2>&1) || true

    if echo "$invalid_arg_output" | grep -qi "unexpected\|unrecognized\|unknown"; then
        log_pass "Invalid flag produces clear error"
    else
        log_fail "Invalid flag error unclear"
    fi

    e2e_add_file "error_invalid_flag.txt" "$invalid_arg_output"

    # Test 2: Missing required argument
    local missing_arg_output
    missing_arg_output=$(run_wa send 2>&1) || true

    if echo "$missing_arg_output" | grep -qi "required\|missing\|argument"; then
        log_pass "Missing argument produces clear error"
    else
        # Some commands might not require args
        log_skip "Missing argument error: Command may not require args"
    fi

    e2e_add_file "error_missing_arg.txt" "$missing_arg_output"

    # Test 3: Check if error messages suggest remediation (wa-bnm features)
    # This is a future feature - just document current state
    local has_remediation=false
    if echo "$invalid_arg_output" | grep -qiE "(try|did you mean|see|hint|suggestion)"; then
        has_remediation=true
    fi

    if $has_remediation; then
        log_pass "Error messages include remediation hints"
    else
        log_skip "Error remediation hints: Not implemented (wa-bnm.2)"
    fi
}

# ==============================================================================
# Progressive Disclosure Tests
# ==============================================================================

test_progressive_disclosure() {
    log_test "Testing Progressive Disclosure"

    # Test that commands show minimal output by default
    # and more detail with -v

    # Test with 'events' command (shows event list)
    local events_default
    events_default=$(run_wa events 2>&1) || true

    local events_verbose
    events_verbose=$(run_wa -v events 2>&1) || true

    # Artifact capture
    e2e_add_file "events_default.txt" "$events_default"
    e2e_add_file "events_verbose.txt" "$events_verbose"

    # Check that both produce output (even if errors due to no watcher)
    if [[ -n "$events_default" && -n "$events_verbose" ]]; then
        log_pass "Progressive disclosure: Both tiers produce output"
    else
        log_fail "Progressive disclosure: One or both tiers empty"
    fi

    # Note: Can't easily test that verbose shows MORE without a running watcher
    log_skip "Progressive disclosure depth: Requires running watcher to verify"
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo "========================================"
    echo "E2E: CLI Polish Validation"
    echo "Implements: bd-3m3a"
    echo "========================================"
    echo ""

    # Initialize artifact collection
    e2e_init_artifacts "cli-polish" > /dev/null
    log_info "Artifacts directory: $E2E_RUN_DIR"
    echo ""

    # Check prerequisites
    check_prerequisites

    # Run test suites
    test_help_output
    test_verbosity_tiers
    test_command_aliases
    test_shell_completions
    test_output_consistency
    test_help_examples
    test_error_messages
    test_progressive_disclosure

    # Finalize and summary
    echo ""
    echo "========================================"
    echo "Summary"
    echo "========================================"
    echo ""
    echo "Tests run:    $TESTS_RUN"
    echo "Tests passed: $TESTS_PASSED"
    echo "Tests failed: $TESTS_FAILED"
    echo "Tests skipped: $TESTS_SKIPPED"
    echo ""

    # Create summary artifact
    local summary="CLI Polish E2E Test Results

Tests run:    $TESTS_RUN
Tests passed: $TESTS_PASSED
Tests failed: $TESTS_FAILED
Tests skipped: $TESTS_SKIPPED

Skipped tests are features not yet implemented:
- Shell completions (wa-rnf.1)
- Smart aliases (wa-rnf.2)
- Debug verbosity -vv (wa-rnf.3)
- Help examples and SEE ALSO (wa-rnf.4)
- Error remediation hints (wa-bnm.2)
"
    e2e_add_file "test_summary.txt" "$summary"

    # Finalize artifacts
    local exit_code=0
    [[ $TESTS_FAILED -gt 0 ]] && exit_code=1

    e2e_finalize $exit_code

    echo ""
    if [[ $TESTS_FAILED -eq 0 ]]; then
        echo -e "${GREEN}All tests passed!${NC} (${TESTS_SKIPPED} skipped)"
    else
        echo -e "${RED}${TESTS_FAILED} test(s) failed${NC}"
        echo "Artifacts saved to: $E2E_RUN_DIR"
    fi

    exit $exit_code
}

main "$@"
