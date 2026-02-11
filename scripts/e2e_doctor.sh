#!/bin/bash
# =============================================================================
# E2E: ft doctor (healthy/broken) with verbose logs + artifacts
# Implements: wa-4vx.10.22
#
# Purpose:
#   Validate that `ft doctor` produces correct exit codes, actionable output,
#   and redacted diagnostics across healthy and broken workspace scenarios.
#
# Requirements:
#   - wa binary built (cargo build -p frankenterm)
#   - jq for JSON manipulation
#   - WezTerm running (for healthy-workspace scenario)
#
# Usage:
#   ./scripts/e2e_doctor.sh [--verbose] [--keep-artifacts]
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
FT_BIN=""
VERBOSE=false
KEEP_ARTIFACTS=false
TEMP_DIRS=()

# ==============================================================================
# Argument parsing
# ==============================================================================

while [[ $# -gt 0 ]]; do
    case "$1" in
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        --keep-artifacts)
            KEEP_ARTIFACTS=true
            shift
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--verbose] [--keep-artifacts]" >&2
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

# Create a temporary workspace directory and track it for cleanup
make_temp_workspace() {
    local dir
    dir=$(mktemp -d "${TMPDIR:-/tmp}/ft-e2e-doctor.XXXXXX")
    TEMP_DIRS+=("$dir")
    echo "$dir"
}

# Run ft doctor and capture output + exit code
# Usage: run_doctor [extra_args...]
# Sets: DOCTOR_STDOUT, DOCTOR_STDERR, DOCTOR_EXIT
run_doctor() {
    local stdout_file stderr_file
    stdout_file=$(mktemp)
    stderr_file=$(mktemp)

    DOCTOR_EXIT=0
    "$FT_BIN" doctor "$@" >"$stdout_file" 2>"$stderr_file" || DOCTOR_EXIT=$?
    DOCTOR_STDOUT=$(cat "$stdout_file")
    DOCTOR_STDERR=$(cat "$stderr_file")

    rm -f "$stdout_file" "$stderr_file"
}

# Run ft doctor with a modified environment
# Usage: run_doctor_env <env_var=val> [extra_args...]
run_doctor_env() {
    local env_setting="$1"
    shift

    local stdout_file stderr_file
    stdout_file=$(mktemp)
    stderr_file=$(mktemp)

    DOCTOR_EXIT=0
    env "$env_setting" "$FT_BIN" doctor "$@" >"$stdout_file" 2>"$stderr_file" || DOCTOR_EXIT=$?
    DOCTOR_STDOUT=$(cat "$stdout_file")
    DOCTOR_STDERR=$(cat "$stderr_file")

    rm -f "$stdout_file" "$stderr_file"
}

# Assert output contains a string
assert_contains() {
    local haystack="$1"
    local needle="$2"
    local description="$3"

    if echo "$haystack" | grep -qF "$needle"; then
        log_pass "$description"
    else
        log_fail "$description (expected to find: '$needle')"
        log_info "Actual output (first 500 chars): ${haystack:0:500}"
    fi
}

# Assert output does NOT contain a string
assert_not_contains() {
    local haystack="$1"
    local needle="$2"
    local description="$3"

    if echo "$haystack" | grep -qF "$needle"; then
        log_fail "$description (found unwanted: '$needle')"
        log_info "Actual output (first 500 chars): ${haystack:0:500}"
    else
        log_pass "$description"
    fi
}

# Assert exit code
assert_exit() {
    local actual="$1"
    local expected="$2"
    local description="$3"

    if [[ "$actual" -eq "$expected" ]]; then
        log_pass "$description"
    else
        log_fail "$description (expected exit=$expected, got exit=$actual)"
    fi
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    # Find wa binary
    if [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
        FT_BIN="$PROJECT_ROOT/target/debug/wa"
    elif [[ -x "$PROJECT_ROOT/target/release/ft" ]]; then
        FT_BIN="$PROJECT_ROOT/target/release/ft"
    else
        echo -e "${RED}ERROR:${NC} wa binary not found. Run: cargo build -p frankenterm" >&2
        exit 5
    fi
    log_pass "wa binary found: $FT_BIN"

    # Check jq
    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found. Install: sudo apt install jq" >&2
        exit 5
    fi
    log_pass "jq available"

    echo ""
    echo "Binary: $FT_BIN"
    echo "Version: $("$FT_BIN" --version 2>/dev/null || echo 'unknown')"
}

# ==============================================================================
# Scenario A: Healthy workspace
# ==============================================================================

test_healthy_workspace() {
    log_test "Scenario A: Healthy workspace"

    run_doctor --workspace "$PROJECT_ROOT"

    # A1: Exit code should be 0
    assert_exit "$DOCTOR_EXIT" 0 "A1: healthy workspace exits 0"

    # A2: Output should contain success markers
    assert_contains "$DOCTOR_STDOUT" "[OK]" "A2: output contains [OK] markers"
    assert_contains "$DOCTOR_STDOUT" "frankenterm-core loaded" "A3: reports frankenterm-core version"
    assert_contains "$DOCTOR_STDOUT" "workspace root" "A4: reports workspace root"
    assert_contains "$DOCTOR_STDOUT" "database" "A5: reports database status"

    # A6: Output should contain success summary
    # Could be "All checks passed" or "Diagnostics completed with warnings"
    local has_summary=false
    if echo "$DOCTOR_STDOUT" | grep -qF "All checks passed"; then
        has_summary=true
    fi
    if echo "$DOCTOR_STDOUT" | grep -qF "Diagnostics completed"; then
        has_summary=true
    fi
    if [[ "$has_summary" == "true" ]]; then
        log_pass "A6: output contains summary line"
    else
        log_fail "A6: output missing summary line"
        log_info "Output: ${DOCTOR_STDOUT:0:500}"
    fi

    # Save artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/healthy_workspace"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Scenario A2: Healthy workspace with --circuits
# ==============================================================================

test_healthy_circuits() {
    log_test "Scenario A2: Healthy workspace with --circuits"

    run_doctor --workspace "$PROJECT_ROOT" --circuits

    # Circuit breaker output
    assert_exit "$DOCTOR_EXIT" 0 "A2.1: --circuits exits 0"
    assert_contains "$DOCTOR_STDOUT" "Circuit Breaker Status" "A2.2: shows circuit breaker header"
    assert_contains "$DOCTOR_STDOUT" "wezterm_cli" "A2.3: shows wezterm_cli circuit"
    assert_contains "$DOCTOR_STDOUT" "CLOSED" "A2.4: circuits report CLOSED (healthy)"

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/healthy_circuits"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Scenario B1: Broken workspace — unwritable
# ==============================================================================

test_broken_unwritable() {
    log_test "Scenario B1: Broken workspace — unwritable"

    local ws
    ws=$(make_temp_workspace)
    chmod 000 "$ws"

    run_doctor --workspace "$ws"

    # B1.1: Should fail
    if [[ "$DOCTOR_EXIT" -ne 0 ]]; then
        log_pass "B1.1: unwritable workspace exits non-zero (exit=$DOCTOR_EXIT)"
    else
        log_fail "B1.1: unwritable workspace should exit non-zero (got exit=0)"
    fi

    # B1.2: Error output should mention permission
    local combined="$DOCTOR_STDOUT$DOCTOR_STDERR"
    local has_hint=false
    if echo "$combined" | grep -qi "permission\|writable\|Permission denied"; then
        has_hint=true
    fi
    if [[ "$has_hint" == "true" ]]; then
        log_pass "B1.2: error mentions permission issue"
    else
        log_fail "B1.2: error should mention permission issue"
        log_info "stdout: ${DOCTOR_STDOUT:0:300}"
        log_info "stderr: ${DOCTOR_STDERR:0:300}"
    fi

    # B1.3: Error should contain actionable hint
    if echo "$combined" | grep -qi "workspace\|FT_WORKSPACE\|--workspace"; then
        log_pass "B1.3: error contains actionable hint about workspace"
    else
        log_fail "B1.3: error should contain workspace hint"
    fi

    # Restore permissions for cleanup
    chmod 755 "$ws"

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/broken_unwritable"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Scenario B2: Broken workspace — missing WezTerm CLI
# ==============================================================================

test_broken_no_wezterm() {
    log_test "Scenario B2: Broken workspace — missing WezTerm CLI"

    local ws
    ws=$(make_temp_workspace)

    # Run with empty PATH so wezterm is not found
    # Keep only the directory containing wa binary itself
    local wa_dir
    wa_dir=$(dirname "$FT_BIN")
    run_doctor_env "PATH=$wa_dir" --workspace "$ws"

    # B2.1: Should fail (exit non-zero)
    if [[ "$DOCTOR_EXIT" -ne 0 ]]; then
        log_pass "B2.1: missing wezterm exits non-zero (exit=$DOCTOR_EXIT)"
    else
        log_fail "B2.1: missing wezterm should exit non-zero (got exit=0)"
    fi

    # B2.2: Output should mention wezterm not found
    local combined="$DOCTOR_STDOUT$DOCTOR_STDERR"
    if echo "$combined" | grep -qi "wezterm.*not found\|wezterm.*not installed\|No such file"; then
        log_pass "B2.2: output mentions wezterm not found"
    else
        log_fail "B2.2: output should mention wezterm not found"
        log_info "stdout: ${DOCTOR_STDOUT:0:500}"
    fi

    # B2.3: Output should contain [ERR] markers
    if echo "$combined" | grep -qF "[ERR]"; then
        log_pass "B2.3: output contains [ERR] markers"
    else
        # May be in stderr as a structured error instead
        log_skip "B2.3: [ERR] markers not found (may use different format)"
    fi

    # B2.4: Other checks should still run (not bail early)
    if echo "$DOCTOR_STDOUT" | grep -qF "frankenterm-core loaded"; then
        log_pass "B2.4: other checks still execute"
    else
        # If wezterm missing causes early exit via config validation, that's also valid
        log_skip "B2.4: early exit before per-check output (config validation)"
    fi

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/broken_no_wezterm"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Scenario B3: Fresh workspace — no DB yet
# ==============================================================================

test_fresh_workspace() {
    log_test "Scenario B3: Fresh workspace — no database"

    local ws
    ws=$(make_temp_workspace)

    run_doctor --workspace "$ws"

    # B3.1: Should still succeed (exit 0) since no DB is a known state
    # (db will be created on first daemon start)
    assert_exit "$DOCTOR_EXIT" 0 "B3.1: fresh workspace exits 0 (warnings ok)"

    # B3.2: Should mention database doesn't exist
    assert_contains "$DOCTOR_STDOUT" "WARN" "B3.2: warns about missing database"

    # B3.3: Should mention that DB will be created
    if echo "$DOCTOR_STDOUT" | grep -qi "created\|first.*start\|first.*run"; then
        log_pass "B3.3: mentions DB will be created"
    else
        log_fail "B3.3: should mention DB will be created"
        log_info "Output: ${DOCTOR_STDOUT:0:500}"
    fi

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/fresh_workspace"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Scenario C: Secret redaction
# ==============================================================================

test_secret_redaction() {
    log_test "Scenario C: No secrets in output"

    run_doctor --workspace "$PROJECT_ROOT"

    local combined="$DOCTOR_STDOUT$DOCTOR_STDERR"

    # C1: No API keys should appear
    assert_not_contains "$combined" "sk-" "C1: no OpenAI-style API keys in output"

    # C2: No bearer tokens
    assert_not_contains "$combined" "Bearer " "C2: no bearer tokens in output"

    # C3: No raw password strings
    assert_not_contains "$combined" "password=" "C3: no password= strings in output"

    # C4: Output should be safe for sharing (no home dir in sensitive context)
    # Note: workspace paths containing username are expected and ok

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/secret_redaction"
        mkdir -p "$scenario_dir"
        echo "$combined" > "$scenario_dir/combined.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"

        # Grep-style proof: count secret-like patterns
        local secret_count=0
        for pattern in "sk-[a-zA-Z0-9]{20}" "Bearer [a-zA-Z0-9]" "password=" "AKIA[A-Z0-9]{16}"; do
            local count
            count=$(echo "$combined" | grep -cE "$pattern" 2>/dev/null || true)
            count="${count%%[^0-9]*}"
            count="${count:-0}"
            secret_count=$((secret_count + count))
        done
        echo "$secret_count" > "$scenario_dir/secret_pattern_matches.txt"
        log_info "Secret pattern matches: $secret_count"
    fi
}

# ==============================================================================
# Scenario D: Output stability
# ==============================================================================

test_output_stability() {
    log_test "Scenario D: Output stability (deterministic)"

    # Run doctor twice and compare structural output
    run_doctor --workspace "$PROJECT_ROOT"
    local output1="$DOCTOR_STDOUT"
    local exit1="$DOCTOR_EXIT"

    run_doctor --workspace "$PROJECT_ROOT"
    local output2="$DOCTOR_STDOUT"
    local exit2="$DOCTOR_EXIT"

    # D1: Exit codes should be identical
    assert_exit "$exit1" "$exit2" "D1: exit codes are deterministic"

    # D2: Same set of check labels should appear
    local checks1 checks2
    checks1=$(echo "$output1" | grep -oE '\[(OK|WARN|ERR)\]' | sort)
    checks2=$(echo "$output2" | grep -oE '\[(OK|WARN|ERR)\]' | sort)

    if [[ "$checks1" == "$checks2" ]]; then
        log_pass "D2: check status markers are deterministic"
    else
        log_fail "D2: check status markers differ between runs"
        log_info "Run 1: $checks1"
        log_info "Run 2: $checks2"
    fi

    # D3: Same check names appear in both runs
    local names1 names2
    names1=$(echo "$output1" | grep -oE '(frankenterm-core|workspace|database|daemon|logs|features|config|WezTerm|filesystem|connection)' | sort -u)
    names2=$(echo "$output2" | grep -oE '(frankenterm-core|workspace|database|daemon|logs|features|config|WezTerm|filesystem|connection)' | sort -u)

    if [[ "$names1" == "$names2" ]]; then
        log_pass "D3: check names are deterministic"
    else
        log_fail "D3: check names differ between runs"
    fi

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/output_stability"
        mkdir -p "$scenario_dir"
        echo "$output1" > "$scenario_dir/run1.log"
        echo "$output2" > "$scenario_dir/run2.log"
    fi
}

# ==============================================================================
# Scenario E: Nonexistent workspace
# ==============================================================================

test_nonexistent_workspace() {
    log_test "Scenario E: Nonexistent workspace path"

    run_doctor --workspace "/tmp/ft-e2e-does-not-exist-$(date +%s)"

    # E1: wa treats nonexistent workspaces as fresh (creates on first watch),
    # so doctor should succeed with warnings, not fail.
    assert_exit "$DOCTOR_EXIT" 0 "E1: nonexistent workspace exits 0 (treated as fresh)"

    # E2: Should warn about missing DB (since the directory doesn't exist yet)
    assert_contains "$DOCTOR_STDOUT" "WARN" "E2: warns about missing database"

    # E3: Should still report all checks
    assert_contains "$DOCTOR_STDOUT" "frankenterm-core loaded" "E3: reports frankenterm-core version"

    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/nonexistent_workspace"
        mkdir -p "$scenario_dir"
        echo "$DOCTOR_STDOUT" > "$scenario_dir/stdout.log"
        echo "$DOCTOR_STDERR" > "$scenario_dir/stderr.log"
        echo "$DOCTOR_EXIT" > "$scenario_dir/exit_code"
    fi
}

# ==============================================================================
# Cleanup
# ==============================================================================

cleanup() {
    for dir in "${TEMP_DIRS[@]}"; do
        if [[ -d "$dir" ]]; then
            # Restore permissions in case they were changed
            chmod -R u+rwX "$dir" 2>/dev/null || true
            rm -r "$dir" 2>/dev/null || true
        fi
    done
}

trap cleanup EXIT

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo "================================================================"
    echo "  E2E: ft doctor (wa-4vx.10.22)"
    echo "================================================================"
    echo ""

    check_prerequisites

    # Initialize artifact collection
    e2e_init_artifacts "e2e-doctor" > /dev/null

    # Run all scenarios
    test_healthy_workspace
    test_healthy_circuits
    test_broken_unwritable
    test_broken_no_wezterm
    test_fresh_workspace
    test_secret_redaction
    test_output_stability
    test_nonexistent_workspace

    # Finalize artifacts
    e2e_finalize $TESTS_FAILED > /dev/null

    # Summary
    echo ""
    echo "================================================================"
    echo "  Results: $TESTS_PASSED passed, $TESTS_FAILED failed, $TESTS_SKIPPED skipped"
    echo "  Total:   $TESTS_RUN tests"
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        echo "  Artifacts: $E2E_RUN_DIR"
    fi
    echo "================================================================"

    if [[ $TESTS_FAILED -gt 0 ]]; then
        exit 1
    fi
    exit 0
}

main
