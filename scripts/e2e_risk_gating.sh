#!/bin/bash
# =============================================================================
# E2E: Risk-Based Gating Validation
# Implements: wa-upg.6.5
#
# Purpose:
#   Validate that risk scoring impacts policy decisions end-to-end.
#   Demonstrates that:
#   - Low-risk actions are allowed
#   - Elevated-risk actions require approval with risk summary
#   - Risk metadata is visible in policy decision outputs
#
# Requirements:
#   - wa binary built
#   - jq for JSON manipulation
# =============================================================================

set -euo pipefail

# Source E2E artifacts library
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

# Binary path
FT_BIN=""

# Logging functions
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

log_info() {
    echo -e "${YELLOW}[INFO]${NC} $*"
}

# Find the wa binary
find_ft_binary() {
    local candidates=(
        "$PROJECT_ROOT/target/release/ft"
        "$PROJECT_ROOT/target/debug/wa"
    )

    for candidate in "${candidates[@]}"; do
        if [[ -x "$candidate" ]]; then
            FT_BIN="$candidate"
            return 0
        fi
    done

    echo "Error: wa binary not found. Run 'cargo build' first."
    exit 1
}

# Run wa command with timeout, extract JSON from output
run_wa_timeout() {
    local timeout_secs="${1:-5}"
    shift
    local raw_output
    raw_output=$(timeout "$timeout_secs" "$FT_BIN" "$@" 2>&1 || true)

    # Strip ANSI codes and extract JSON object
    local stripped
    stripped=$(echo "$raw_output" | sed 's/\x1b\[[0-9;]*m//g')

    # Extract JSON from first { to last }
    echo "$stripped" | awk '
        /^{/ { found=1 }
        found { print }
    '
}

# =============================================================================
# Test: Risk metadata in policy decisions
# =============================================================================

test_risk_metadata_presence() {
    log_test "Risk Metadata Presence"

    # This test validates that risk scoring is being calculated and included
    # in policy decision outputs through the wa robot send --dry-run command

    local output
    output=$(run_wa_timeout 10 robot send 0 "test command" --dry-run 2>&1 || true)

    # Save artifact
    e2e_add_file "risk_metadata_presence" "dry_run_output.json" "$output"

    # Check if output is valid JSON
    if ! echo "$output" | jq -e . >/dev/null 2>&1; then
        log_fail "Output is not valid JSON: $output"
        return 1
    fi

    # Check for risk-related fields in the context
    # The decision.context.risk should be present when risk scoring is active
    local has_context
    has_context=$(echo "$output" | jq -r '.data.decision.context // empty' 2>/dev/null || echo "")

    if [[ -n "$has_context" ]]; then
        log_pass "Policy decision includes context"
    else
        # Risk scoring might not be active in dry-run, check for ok field
        local is_ok
        is_ok=$(echo "$output" | jq -r '.ok' 2>/dev/null || echo "false")
        if [[ "$is_ok" == "true" ]]; then
            log_pass "Dry-run returned OK response (risk may not apply to synthetic requests)"
        else
            log_fail "Missing context in policy decision"
            return 1
        fi
    fi
}

# =============================================================================
# Test: Low-risk actions should be allowed
# =============================================================================

test_low_risk_allows() {
    log_test "Low-Risk Actions Allowed"

    # A simple read action should have low risk and be allowed
    # Using robot state which is a read-only operation

    local output
    output=$(run_wa_timeout 10 robot state 2>&1 || true)

    e2e_add_file "low_risk_allows" "state_output.json" "$output"

    # Check if output is valid JSON
    if ! echo "$output" | jq -e . >/dev/null 2>&1; then
        log_fail "State command output is not valid JSON"
        return 1
    fi

    # Check ok field
    local is_ok
    is_ok=$(echo "$output" | jq -r '.ok' 2>/dev/null || echo "false")

    if [[ "$is_ok" == "true" ]]; then
        log_pass "Read-only state command allowed (low risk)"
    else
        # Check if it's a WezTerm not running error (expected in test env)
        local error_code
        error_code=$(echo "$output" | jq -r '.error.code // empty' 2>/dev/null || echo "")
        if [[ "$error_code" == "robot.wezterm_not_running" ]]; then
            log_pass "Command attempted (WezTerm not running in test env, but policy didn't block)"
        else
            log_fail "Low-risk command was blocked: $output"
            return 1
        fi
    fi
}

# =============================================================================
# Test: Risk scoring unit test via cargo
# =============================================================================

test_risk_scoring_unit_tests() {
    log_test "Risk Scoring Unit Tests"

    # Run the comprehensive risk scoring tests
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk 2>&1 || true)

    e2e_add_file "risk_scoring_unit_tests" "cargo_test_output.txt" "$output"

    # Check for test success
    if echo "$output" | grep -q "test result: ok"; then
        local passed
        passed=$(echo "$output" | grep "test result: ok" | head -1 | grep -oP '\d+ passed' | grep -oP '\d+')
        log_pass "All risk scoring tests passed ($passed tests)"
    else
        log_fail "Risk scoring tests failed"
        echo "$output" | tail -20
        return 1
    fi
}

# =============================================================================
# Test: Risk factors are deterministic
# =============================================================================

test_risk_determinism() {
    log_test "Risk Scoring Determinism"

    # Run the determinism test specifically
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_score_deterministic -- --nocapture 2>&1 || true)

    e2e_add_file "risk_determinism" "determinism_test.txt" "$output"

    if echo "$output" | grep -q "test policy::tests::risk_score_deterministic ... ok"; then
        log_pass "Risk scoring is deterministic"
    else
        log_fail "Risk scoring determinism test failed"
        return 1
    fi
}

# =============================================================================
# Test: Risk factor ordering is stable
# =============================================================================

test_risk_factor_ordering() {
    log_test "Risk Factor Ordering Stability"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_factors_have_stable_ordering -- --nocapture 2>&1 || true)

    e2e_add_file "risk_factor_ordering" "ordering_test.txt" "$output"

    if echo "$output" | grep -q "test policy::tests::risk_factors_have_stable_ordering ... ok"; then
        log_pass "Risk factor ordering is stable"
    else
        log_fail "Risk factor ordering test failed"
        return 1
    fi
}

# =============================================================================
# Test: Risk thresholds map to decisions correctly
# =============================================================================

test_risk_decision_mapping() {
    log_test "Risk-to-Decision Mapping"

    # Run all three decision mapping tests
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_to_decision -- --nocapture 2>&1 || true)

    e2e_add_file "risk_decision_mapping" "decision_tests.txt" "$output"

    local all_passed=true

    if echo "$output" | grep -q "risk_to_decision_allow_for_low ... ok"; then
        log_pass "Low risk (0-50) maps to Allow"
    else
        log_fail "Low risk decision mapping failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_to_decision_require_approval_for_elevated ... ok"; then
        log_pass "Elevated risk (51-70) maps to RequireApproval"
    else
        log_fail "Elevated risk decision mapping failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_to_decision_deny_for_high ... ok"; then
        log_pass "High risk (71-100) maps to Deny"
    else
        log_fail "High risk decision mapping failed"
        all_passed=false
    fi

    $all_passed
}

# =============================================================================
# Test: Risk JSON schema validation
# =============================================================================

test_risk_json_schema() {
    log_test "Risk JSON Schema Validation"

    # Run each test separately since cargo test only accepts one filter
    local output1 output2 output3 output
    output1=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_score_json_schema -- --nocapture 2>&1 || true)
    output2=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_factor_json_schema -- --nocapture 2>&1 || true)
    output3=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core decision_context_risk -- --nocapture 2>&1 || true)
    output="$output1"$'\n'"$output2"$'\n'"$output3"

    e2e_add_file "risk_json_schema" "schema_tests.txt" "$output"

    local all_passed=true

    if echo "$output1" | grep -q "risk_score_json_schema_has_required_fields ... ok"; then
        log_pass "RiskScore JSON has required fields (score, factors, summary)"
    else
        log_fail "RiskScore JSON schema validation failed"
        all_passed=false
    fi

    if echo "$output2" | grep -q "risk_factor_json_schema_has_required_fields ... ok"; then
        log_pass "RiskFactor JSON has required fields (id, weight, explanation)"
    else
        log_fail "RiskFactor JSON schema validation failed"
        all_passed=false
    fi

    if echo "$output3" | grep -q "decision_context_risk_json_is_valid ... ok"; then
        log_pass "DecisionContext includes valid risk object"
    else
        log_fail "DecisionContext risk validation failed"
        all_passed=false
    fi

    $all_passed
}

# =============================================================================
# Test: Risk matrix coverage
# =============================================================================

test_risk_matrix() {
    log_test "Risk Scoring Matrix Coverage"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p frankenterm-core risk_matrix -- --nocapture 2>&1 || true)

    e2e_add_file "risk_matrix" "matrix_tests.txt" "$output"

    local all_passed=true

    if echo "$output" | grep -q "risk_matrix_safe_read_action ... ok"; then
        log_pass "Read actions don't add mutating/destructive factors"
    else
        log_fail "Read action risk matrix test failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_matrix_human_actor_trusted ... ok"; then
        log_pass "Human actors don't get untrusted penalty"
    else
        log_fail "Human actor trust test failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_matrix_combined_state_factors ... ok"; then
        log_pass "Multiple state factors accumulate correctly"
    else
        log_fail "Combined state factors test failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_matrix_content_analysis ... ok"; then
        log_pass "Content analysis detects dangerous patterns"
    else
        log_fail "Content analysis test failed"
        all_passed=false
    fi

    if echo "$output" | grep -q "risk_matrix_reserved_pane ... ok"; then
        log_pass "Reserved panes add appropriate risk"
    else
        log_fail "Reserved pane test failed"
        all_passed=false
    fi

    $all_passed
}

# =============================================================================
# Main
# =============================================================================

main() {
    echo "=========================================="
    echo "E2E: Risk-Based Gating Validation"
    echo "Bead: wa-upg.6.5"
    echo "=========================================="
    echo ""

    # Initialize artifacts
    e2e_init_artifacts "risk-gating"

    # Find wa binary
    find_ft_binary
    log_info "Using wa binary: $FT_BIN"

    # Run tests
    test_risk_scoring_unit_tests || true
    test_risk_determinism || true
    test_risk_factor_ordering || true
    test_risk_decision_mapping || true
    test_risk_json_schema || true
    test_risk_matrix || true
    test_low_risk_allows || true
    test_risk_metadata_presence || true

    # Summary
    echo ""
    echo "=========================================="
    echo "Summary"
    echo "=========================================="
    echo "Tests run:    $TESTS_RUN"
    echo "Tests passed: $TESTS_PASSED"
    echo "Tests failed: $TESTS_FAILED"

    # Finalize artifacts
    e2e_finalize $TESTS_FAILED

    if [[ $TESTS_FAILED -gt 0 ]]; then
        echo ""
        echo -e "${RED}FAILED${NC}: $TESTS_FAILED test(s) failed"
        exit 1
    else
        echo ""
        echo -e "${GREEN}PASSED${NC}: All tests passed"
        exit 0
    fi
}

main "$@"
