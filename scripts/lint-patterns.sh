#!/usr/bin/env bash
# lint-patterns.sh - Enforce pattern code organization via ast-grep
#
# This script enforces:
# 1. No ad-hoc Regex::new() outside allowed modules
# 2. Detection patterns centralized in pattern packs
#
# Usage:
#   ./scripts/lint-patterns.sh             # Lint all Rust files
#   ./scripts/lint-patterns.sh --check     # Check mode (exit 1 on violations)
#   ./scripts/lint-patterns.sh --self-test # Verify lint rules work correctly
#   ./scripts/lint-patterns.sh --help      # Show help
#
# Allowed files for Regex::new:
#   - crates/frankenterm-core/src/patterns.rs  (pattern detection engine)
#   - crates/frankenterm-core/src/policy.rs    (command safety, secret detection)
#   - crates/frankenterm-core/src/config.rs    (configuration parsing)

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# Allowed files for Regex::new usage
ALLOWED_REGEX_FILES=(
    "crates/frankenterm-core/src/patterns.rs"
    "crates/frankenterm-core/src/policy.rs"
    "crates/frankenterm-core/src/config.rs"
)

# Test fixtures are allowed to use regex for testing purposes
ALLOWED_REGEX_PATTERNS=(
    "*/tests/*.rs"
    "*/benches/*.rs"
    "*_test.rs"
)

usage() {
    echo "Usage: $0 [--check] [--self-test] [--help] [FILES...]"
    echo ""
    echo "Lint Rust files for pattern code organization violations."
    echo ""
    echo "Options:"
    echo "  --check      Check mode - exit with error if violations found"
    echo "  --self-test  Verify lint rules work by checking test fixtures"
    echo "  --help       Show this help message"
    echo "  FILES...     Specific files to lint (default: all *.rs in crates/)"
    echo ""
    echo "Allowed files for Regex::new:"
    for f in "${ALLOWED_REGEX_FILES[@]}"; do
        echo "  - $f"
    done
}

# Self-test function: verify rules catch violations
run_self_test() {
    echo "Running lint rule self-tests..."
    echo ""

    local passed=0
    local failed=0

    # Test 1: should_fail fixture must be caught
    echo "Test 1: Verifying bad fixture triggers violation..."
    local bad_fixture="rules/tests/should_fail_adhoc_regex.rs"
    if [[ -f "$bad_fixture" ]]; then
        local output
        output=$(ast-grep scan --rule rules/no-adhoc-regex.yml "$bad_fixture" 2>&1 || true)
        if echo "$output" | grep -q "error\[no-adhoc"; then
            echo -e "  ${GREEN}PASS${NC}: Bad fixture correctly flagged"
            passed=$((passed + 1))
        else
            echo -e "  ${RED}FAIL${NC}: Bad fixture not caught by lint"
            failed=$((failed + 1))
        fi
    else
        echo -e "  ${YELLOW}SKIP${NC}: Bad fixture not found: $bad_fixture"
    fi

    # Test 2: allowed files should not be checked (via script filtering)
    echo "Test 2: Verifying allowlist filtering works..."
    if is_allowed_regex_file "crates/frankenterm-core/src/patterns.rs"; then
        echo -e "  ${GREEN}PASS${NC}: patterns.rs correctly in allowlist"
        ((passed++))
    else
        echo -e "  ${RED}FAIL${NC}: patterns.rs not in allowlist"
        ((failed++))
    fi

    if is_allowed_regex_file "crates/frankenterm-core/src/policy.rs"; then
        echo -e "  ${GREEN}PASS${NC}: policy.rs correctly in allowlist"
        ((passed++))
    else
        echo -e "  ${RED}FAIL${NC}: policy.rs not in allowlist"
        ((failed++))
    fi

    # Test 3: non-allowed file should not be in allowlist
    echo "Test 3: Verifying non-allowed files are checked..."
    if ! is_allowed_regex_file "crates/frankenterm-core/src/workflows.rs"; then
        echo -e "  ${GREEN}PASS${NC}: workflows.rs correctly not in allowlist"
        ((passed++))
    else
        echo -e "  ${RED}FAIL${NC}: workflows.rs incorrectly in allowlist"
        ((failed++))
    fi

    echo ""
    echo "----------------------------------------"
    echo "Self-test results: ${passed} passed, ${failed} failed"

    if [[ $failed -gt 0 ]]; then
        return 1
    fi
    return 0
}

# Check if file is in allowed list
is_allowed_regex_file() {
    local file="$1"

    # Check exact matches
    for allowed in "${ALLOWED_REGEX_FILES[@]}"; do
        if [[ "$file" == *"$allowed" ]]; then
            return 0
        fi
    done

    # Check patterns (tests, benches)
    for pattern in "${ALLOWED_REGEX_PATTERNS[@]}"; do
        # shellcheck disable=SC2053
        if [[ "$file" == $pattern ]]; then
            return 0
        fi
    done

    return 1
}

# Main lint function
lint_files() {
    local check_mode="${1:-false}"
    shift || true
    local files=("$@")

    local violations=0
    local checked=0

    # If no files specified, find all Rust files in crates
    if [[ ${#files[@]} -eq 0 ]]; then
        mapfile -t files < <(find crates -name "*.rs" -type f 2>/dev/null || true)
    fi

    # Check for ast-grep
    if ! command -v ast-grep &> /dev/null; then
        echo -e "${RED}Error: ast-grep not found. Install with: cargo install ast-grep${NC}"
        exit 1
    fi

    # Filter out allowed files for regex check
    local files_to_check=()
    for file in "${files[@]}"; do
        if ! is_allowed_regex_file "$file"; then
            files_to_check+=("$file")
        fi
    done

    if [[ ${#files_to_check[@]} -eq 0 ]]; then
        echo -e "${GREEN}No files to check (all files in allowlist)${NC}"
        return 0
    fi

    echo "Checking ${#files_to_check[@]} files for pattern violations..."
    echo ""

    # Run ast-grep on each file
    for file in "${files_to_check[@]}"; do
        if [[ ! -f "$file" ]]; then
            continue
        fi

        checked=$((checked + 1))

        # Run ast-grep and capture output
        local output
        output=$(ast-grep scan --rule rules/no-adhoc-regex.yml "$file" 2>&1 || true)

        if [[ -n "$output" ]] && [[ "$output" != *"0 error"* ]]; then
            violations=$((violations + 1))
            echo -e "${RED}Violation in: $file${NC}"
            echo "$output"
            echo ""
        fi
    done

    echo "----------------------------------------"
    echo -e "Checked: ${checked} files"

    if [[ $violations -gt 0 ]]; then
        echo -e "${RED}Violations: ${violations}${NC}"
        echo ""
        echo "Fix:"
        echo "  - Move detection patterns to patterns.rs using RuleDef/PatternPack"
        echo "  - Or add file to allowlist if this is a legitimate use case"

        if [[ "$check_mode" == "true" ]]; then
            return 1
        fi
    else
        echo -e "${GREEN}No violations found${NC}"
    fi

    return 0
}

# Parse arguments
check_mode=false
self_test=false
files=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)
            check_mode=true
            shift
            ;;
        --self-test)
            self_test=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            files+=("$1")
            shift
            ;;
    esac
done

# Run self-test or linting
if [[ "$self_test" == "true" ]]; then
    run_self_test
else
    lint_files "$check_mode" "${files[@]}"
fi
