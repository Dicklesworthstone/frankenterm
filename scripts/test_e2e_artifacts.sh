#!/bin/bash
# Test script for e2e_artifacts.sh library
# Verifies: bd-35zb
#
# Usage: ./scripts/test_e2e_artifacts.sh
#
# This script tests the E2E artifact packer library to ensure:
# - Artifacts are captured correctly
# - Manifests are generated
# - Redaction works
# - Size limiting works

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Source the library
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}[PASS]${NC} $*"; }
fail() { echo -e "${RED}[FAIL]${NC} $*"; exit 1; }
info() { echo -e "${YELLOW}[INFO]${NC} $*"; }

# Test functions
test_passing_scenario() {
    echo "Output from passing scenario"
    echo "Line 1: Everything is working"
    echo "Line 2: No errors here"
    return 0
}

test_failing_scenario() {
    echo "Output from failing scenario"
    echo "Line 1: Starting work..."
    echo "ERROR: Something went wrong!" >&2
    return 1
}

test_with_secrets() {
    echo "Config loaded:"
    echo "OPENAI_API_KEY=sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx234yz"
    echo "password=supersecret123"
    echo "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"
    return 0
}

test_large_output() {
    # Generate about 50KB of output
    for i in {1..1000}; do
        echo "Line $i: This is line number $i with some padding to make it longer xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    done
    return 0
}

# Main test
main() {
    echo "==========================================="
    echo "Testing E2E Artifact Packer Library"
    echo "==========================================="
    echo ""

    # Use a temp directory for testing
    export E2E_ARTIFACTS_BASE="$(mktemp -d /tmp/e2e-artifacts-test-XXXXXX)"
    export E2E_MAX_FILE_SIZE=5000  # 5KB for testing truncation

    info "Using artifacts base: $E2E_ARTIFACTS_BASE"
    echo ""

    # Test 1: Initialize artifacts
    info "Test 1: Initialize artifacts"
    # Call directly (not in subshell) to preserve global state
    e2e_init_artifacts "artifact-packer-test" > /dev/null
    local run_dir="$E2E_RUN_DIR"

    if [[ -d "$run_dir" ]]; then
        pass "Run directory created: $run_dir"
    else
        fail "Run directory not created"
    fi

    if [[ -f "$run_dir/env.json" ]]; then
        pass "Environment snapshot created"
    else
        fail "Environment snapshot missing"
    fi
    echo ""

    # Test 2: Capture passing scenario
    info "Test 2: Capture passing scenario"
    if e2e_capture_scenario "test_pass" test_passing_scenario; then
        pass "Passing scenario captured"
    else
        fail "Passing scenario should have succeeded"
    fi

    if [[ -f "$run_dir/scenarios/test_pass/stdout.log" ]]; then
        pass "Stdout captured"
    else
        fail "Stdout not captured"
    fi

    if [[ -f "$run_dir/scenarios/test_pass/PASS" ]]; then
        pass "PASS marker created"
    else
        fail "PASS marker missing"
    fi
    echo ""

    # Test 3: Capture failing scenario
    info "Test 3: Capture failing scenario"
    if ! e2e_capture_scenario "test_fail" test_failing_scenario; then
        pass "Failing scenario captured with non-zero exit"
    else
        fail "Failing scenario should have failed"
    fi

    if [[ -f "$run_dir/scenarios/test_fail/FAIL" ]]; then
        pass "FAIL marker created"
    else
        fail "FAIL marker missing"
    fi

    if [[ -f "$run_dir/scenarios/test_fail/stderr.log" ]] && grep -q "ERROR" "$run_dir/scenarios/test_fail/stderr.log"; then
        pass "Stderr captured with error"
    else
        fail "Stderr not captured properly"
    fi
    echo ""

    # Test 4: Secret redaction
    info "Test 4: Secret redaction"
    e2e_capture_scenario "test_secrets" test_with_secrets || true

    if grep -q "\[REDACTED\]" "$run_dir/scenarios/test_secrets/stdout.log"; then
        pass "Secrets were redacted"
    else
        fail "Secrets not redacted"
    fi

    if grep -q "sk-proj-abc123" "$run_dir/scenarios/test_secrets/stdout.log"; then
        fail "OpenAI key NOT redacted (security issue!)"
    else
        pass "OpenAI key properly redacted"
    fi

    if grep -q "supersecret123" "$run_dir/scenarios/test_secrets/stdout.log"; then
        fail "Password NOT redacted (security issue!)"
    else
        pass "Password properly redacted"
    fi
    echo ""

    # Test 5: Size limiting
    info "Test 5: Size limiting (max 5KB)"
    e2e_capture_scenario "test_large" test_large_output || true

    local file_size
    file_size=$(stat -c%s "$run_dir/scenarios/test_large/stdout.log" 2>/dev/null || stat -f%z "$run_dir/scenarios/test_large/stdout.log")

    if [[ $file_size -le $E2E_MAX_FILE_SIZE ]]; then
        pass "File size limited to ${file_size} bytes (max: $E2E_MAX_FILE_SIZE)"
    else
        fail "File size exceeded limit: $file_size > $E2E_MAX_FILE_SIZE"
    fi

    if grep -q "TRUNCATED" "$run_dir/scenarios/test_large/stdout.log"; then
        pass "Truncation notice added"
    else
        fail "Truncation notice missing"
    fi
    echo ""

    # Test 6: Add file helpers
    info "Test 6: Add file helpers"
    echo "custom content" | e2e_add_file "custom.txt"

    if [[ -f "$run_dir/custom.txt" ]]; then
        pass "e2e_add_file works"
    else
        fail "e2e_add_file failed"
    fi

    e2e_add_json "data.json" '{"key": "value", "count": 42}'

    if [[ -f "$run_dir/data.json" ]] && jq -e '.key == "value"' "$run_dir/data.json" >/dev/null; then
        pass "e2e_add_json works with valid JSON"
    else
        fail "e2e_add_json failed"
    fi
    echo ""

    # Test 7: Finalize and manifest
    info "Test 7: Finalize and manifest"
    e2e_finalize 0

    if [[ -f "$run_dir/manifest.json" ]]; then
        pass "Manifest created"
    else
        fail "Manifest not created"
    fi

    # Validate manifest structure
    if jq -e '.version and .scenarios and .results' "$run_dir/manifest.json" >/dev/null; then
        pass "Manifest has required fields"
    else
        fail "Manifest missing required fields"
    fi

    local total
    total=$(jq -r '.results.total' "$run_dir/manifest.json")
    local passed
    passed=$(jq -r '.results.passed' "$run_dir/manifest.json")
    local failed
    failed=$(jq -r '.results.failed' "$run_dir/manifest.json")

    info "Results: total=$total passed=$passed failed=$failed"

    if [[ "$total" == "4" && "$passed" == "3" && "$failed" == "1" ]]; then
        pass "Results correctly tracked"
    else
        fail "Results incorrect (expected total=4 passed=3 failed=1)"
    fi

    if [[ -f "$run_dir/summary.txt" ]]; then
        pass "Human-readable summary created"
    else
        fail "Summary not created"
    fi
    echo ""

    # Final summary
    echo "==========================================="
    echo "Test Summary"
    echo "==========================================="
    echo ""
    echo "Artifacts directory: $run_dir"
    echo ""
    echo "Directory structure:"
    find "$run_dir" -type f | head -20
    echo ""
    echo "Manifest:"
    jq '.' "$run_dir/manifest.json"
    echo ""

    # Cleanup
    info "Cleaning up test artifacts..."
    rm -rf "$E2E_ARTIFACTS_BASE"

    echo ""
    echo -e "${GREEN}All tests passed!${NC}"
}

main "$@"
