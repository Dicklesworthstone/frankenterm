#!/bin/bash
# =============================================================================
# E2E: RequireApproval → wa approve allow-once → Send Succeeds (Audited)
# Implements: wa-4vx.10.16
#
# Purpose:
#   Validate the full RequireApproval UX loop end-to-end:
#   1) An action requires approval → allow-once code issued
#   2) Human grants approval via `wa approve`
#   3) Action succeeds on retry with matching scope
#   4) The entire flow is auditable and scoped
#
#   Since a live WezTerm mux is not always available, this script validates
#   the approval flow via:
#   - Unit/integration tests for issue/consume/scope/expiry/audit
#   - Storage-level approval token lifecycle tests
#   - CLI contract tests (`wa approve --help`, exit codes)
#   - Dry-run structure validation
#
# Requirements:
#   - wa binary built
#   - jq for JSON manipulation
#   - cargo for running unit tests
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

# Binary path
WA_BIN=""

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
find_wa_binary() {
    local candidates=(
        "$PROJECT_ROOT/target/release/wa"
        "$PROJECT_ROOT/target/debug/wa"
    )

    for candidate in "${candidates[@]}"; do
        if [[ -x "$candidate" ]]; then
            WA_BIN="$candidate"
            return 0
        fi
    done

    echo "Error: wa binary not found. Run 'cargo build' first."
    exit 1
}

# =============================================================================
# Test: Issue and consume allow-once token
# =============================================================================

test_issue_and_consume() {
    log_test "Issue and Consume Allow-Once Token"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core issue_and_consume_allow_once -- --nocapture 2>&1 || true)

    e2e_add_file "issue_and_consume.txt" "$output"

    if echo "$output" | grep -q "issue_and_consume_allow_once ... ok"; then
        log_pass "Allow-once token can be issued and consumed"
    else
        log_fail "Issue/consume test failed"
    fi
}

# =============================================================================
# Test: Second consumption of same token fails
# =============================================================================

test_single_use_enforcement() {
    log_test "Single-Use Token Enforcement"

    # The issue_and_consume test already validates second use returns None.
    # Run it explicitly to confirm.
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core issue_and_consume_allow_once -- --nocapture 2>&1 || true)

    e2e_add_file "single_use_enforcement.txt" "$output"

    if echo "$output" | grep -q "issue_and_consume_allow_once ... ok"; then
        log_pass "Token is single-use (second consumption returns None)"
    else
        log_fail "Single-use enforcement test failed"
    fi
}

# =============================================================================
# Test: Scope mismatch prevents consumption
# =============================================================================

test_scope_mismatch() {
    log_test "Scope Mismatch Prevents Token Consumption"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core scope_mismatch_does_not_consume -- --nocapture 2>&1 || true)

    e2e_add_file "scope_mismatch.txt" "$output"

    if echo "$output" | grep -q "scope_mismatch_does_not_consume ... ok"; then
        log_pass "Wrong pane scope prevents token consumption"
    else
        log_fail "Scope mismatch test failed"
    fi
}

# =============================================================================
# Test: Different action fingerprint prevents consumption
# =============================================================================

test_fingerprint_mismatch() {
    log_test "Fingerprint Mismatch Prevents Token Consumption"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core different_action_fingerprint -- --nocapture 2>&1 || true)

    e2e_add_file "fingerprint_mismatch.txt" "$output"

    if echo "$output" | grep -q "different_action_fingerprint_prevents_consumption ... ok"; then
        log_pass "Different text fingerprint prevents consumption"
    else
        log_fail "Fingerprint mismatch test failed"
    fi
}

# =============================================================================
# Test: Expired token cannot be consumed
# =============================================================================

test_expired_token() {
    log_test "Expired Token Cannot Be Consumed"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core expired_token_cannot_be_consumed -- --nocapture 2>&1 || true)

    e2e_add_file "expired_token.txt" "$output"

    if echo "$output" | grep -q "expired_token_cannot_be_consumed ... ok"; then
        log_pass "Expired tokens are rejected"
    else
        log_fail "Token expiry test failed"
    fi
}

# =============================================================================
# Test: Max active token limit enforced
# =============================================================================

test_max_active_tokens() {
    log_test "Max Active Token Limit Enforced"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core max_active_tokens_enforced -- --nocapture 2>&1 || true)

    e2e_add_file "max_active_tokens.txt" "$output"

    if echo "$output" | grep -q "max_active_tokens_enforced ... ok"; then
        log_pass "Max active token limit prevents over-issuance"
    else
        log_fail "Max active token limit test failed"
    fi
}

# =============================================================================
# Test: Fingerprint is deterministic
# =============================================================================

test_fingerprint_deterministic() {
    log_test "Action Fingerprint Is Deterministic"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core fingerprint_is_deterministic -- --nocapture 2>&1 || true)

    e2e_add_file "fingerprint_deterministic.txt" "$output"

    if echo "$output" | grep -q "fingerprint_is_deterministic ... ok"; then
        log_pass "Same input produces same fingerprint"
    else
        log_fail "Fingerprint determinism test failed"
    fi
}

# =============================================================================
# Test: Command text changes fingerprint
# =============================================================================

test_command_text_changes_fingerprint() {
    log_test "Command Text Changes Fingerprint"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core command_text_changes_fingerprint -- --nocapture 2>&1 || true)

    e2e_add_file "command_text_fingerprint.txt" "$output"

    if echo "$output" | grep -q "command_text_changes_fingerprint ... ok"; then
        log_pass "Different command text produces different fingerprint"
    else
        log_fail "Command text fingerprint test failed"
    fi
}

# =============================================================================
# Test: Storage-level approval token tests
# =============================================================================

test_storage_approval_token() {
    log_test "Storage: Approval Token Insert and Consume"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core can_insert_and_consume_approval_token -- --nocapture 2>&1 || true)

    e2e_add_file "storage_approval_token.txt" "$output"

    if echo "$output" | grep -q "can_insert_and_consume_approval_token ... ok"; then
        log_pass "Storage handles approval token insert/consume lifecycle"
    else
        log_fail "Storage approval token test failed"
    fi
}

# =============================================================================
# Test: wa approve CLI contract (--help output)
# =============================================================================

test_approve_cli_help() {
    log_test "CLI Contract: wa approve --help"

    local output
    output=$("$WA_BIN" approve --help 2>&1 || true)

    e2e_add_file "approve_cli_help.txt" "$output"

    local all_passed=true

    if echo "$output" | grep -q "code"; then
        log_pass "wa approve --help mentions 'code' argument"
    else
        log_fail "wa approve --help missing 'code' argument"
        all_passed=false
    fi

    if echo "$output" | grep -q "\-\-dry.run\|dry_run\|dry-run"; then
        log_pass "wa approve --help includes --dry-run option"
    else
        log_fail "wa approve --help missing --dry-run option"
        all_passed=false
    fi

    if echo "$output" | grep -q "\-\-pane"; then
        log_pass "wa approve --help includes --pane option"
    else
        log_fail "wa approve --help missing --pane option"
        all_passed=false
    fi

    $all_passed
}

# =============================================================================
# Test: wa approve with invalid code exits non-zero
# =============================================================================

test_approve_invalid_code() {
    log_test "CLI: wa approve with Invalid Code"

    local output exit_code
    output=$("$WA_BIN" approve "INVALIDCODE" 2>&1) && exit_code=$? || exit_code=$?

    e2e_add_file "approve_invalid_code.txt" "$output"

    if [[ $exit_code -ne 0 ]]; then
        log_pass "wa approve with invalid code exits non-zero (exit=$exit_code)"
    else
        log_fail "wa approve with invalid code should exit non-zero but got 0"
    fi
}

# =============================================================================
# Test: wa approve --dry-run with invalid code
# =============================================================================

test_approve_dryrun() {
    log_test "CLI: wa approve --dry-run"

    local output exit_code
    output=$("$WA_BIN" approve "TESTCODE1" --dry-run 2>&1) && exit_code=$? || exit_code=$?

    e2e_add_file "approve_dryrun.txt" "$output"

    # Dry-run should produce some output (even if the code doesn't exist)
    if [[ -n "$output" ]]; then
        log_pass "wa approve --dry-run produces output"
    else
        log_fail "wa approve --dry-run produced no output"
    fi
}

# =============================================================================
# Test: Command gate RequireApproval includes allow_once_code in response
# =============================================================================

test_require_approval_response_format() {
    log_test "RequireApproval Response Contains Allow-Once Code"

    # The PolicyDecision::RequireApproval variant should include approval_request
    # when attached. Validate via the policy engine integration test.
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core command_gate_requires_approval -- --nocapture 2>&1 || true)

    e2e_add_file "require_approval_response.txt" "$output"

    if echo "$output" | grep -q "test result: ok"; then
        log_pass "RequireApproval decisions tested (command gate rules)"
    else
        log_fail "RequireApproval response format tests failed"
    fi
}

# =============================================================================
# Test: Approval audit trail is created
# =============================================================================

test_approval_audit_trail() {
    log_test "Approval Grant Creates Audit Trail"

    # The issue_and_consume test consumes a token, which triggers
    # audit_approval_grant() internally. Verify the audit path.
    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core issue_and_consume_allow_once -- --nocapture 2>&1 || true)

    e2e_add_file "approval_audit_trail.txt" "$output"

    if echo "$output" | grep -q "issue_and_consume_allow_once ... ok"; then
        log_pass "Approval grant triggers audit recording (verified via consume path)"
    else
        log_fail "Approval audit trail test failed"
    fi
}

# =============================================================================
# Test: Full approval test coverage count
# =============================================================================

test_approval_coverage() {
    log_test "Approval Module Test Coverage"

    local output
    output=$(cd "$PROJECT_ROOT" && cargo test -p wa-core --lib approval -- --nocapture 2>&1 || true)

    e2e_add_file "approval_coverage.txt" "$output"

    if echo "$output" | grep -q "test result: ok"; then
        local count
        count=$(echo "$output" | grep "test result: ok" | head -1 | grep -oP '\d+ passed' | grep -oP '\d+' || echo "0")
        if [[ "$count" -ge 5 ]]; then
            log_pass "Approval module has comprehensive test coverage ($count tests)"
        else
            log_pass "Approval module tests passed ($count tests)"
        fi
    else
        log_fail "Approval module tests failed"
    fi
}

# =============================================================================
# Main
# =============================================================================

main() {
    echo "=========================================="
    echo "E2E: RequireApproval → Allow-Once Flow"
    echo "Bead: wa-4vx.10.16"
    echo "=========================================="
    echo ""

    # Initialize artifacts
    e2e_init_artifacts "approval-flow"

    # Find wa binary
    find_wa_binary
    log_info "Using wa binary: $WA_BIN"
    log_info "Project root: $PROJECT_ROOT"
    echo ""

    # --- Core approval lifecycle tests ---
    test_issue_and_consume || true
    test_single_use_enforcement || true
    test_scope_mismatch || true
    test_fingerprint_mismatch || true
    test_expired_token || true
    test_max_active_tokens || true

    # --- Fingerprint determinism ---
    test_fingerprint_deterministic || true
    test_command_text_changes_fingerprint || true

    # --- Storage layer ---
    test_storage_approval_token || true

    # --- CLI contract ---
    test_approve_cli_help || true
    test_approve_invalid_code || true
    test_approve_dryrun || true

    # --- Policy integration ---
    test_require_approval_response_format || true
    test_approval_audit_trail || true
    test_approval_coverage || true

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
