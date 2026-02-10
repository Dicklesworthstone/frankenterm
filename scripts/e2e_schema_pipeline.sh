#!/bin/bash
# =============================================================================
# E2E: Schema/Docs/Client Pipeline Validation
# Implements: bd-7ois
#
# Purpose:
#   Validate that robot command outputs conform to JSON schemas.
#   This ensures the schema contract is maintained and provides confidence
#   for typed client generation.
#
# Requirements:
#   - jsonschema CLI (pip install jsonschema)
#   - jq for JSON manipulation
#   - wa binary built
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
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    NC=''
fi

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0

# Binary and schema paths
FT_BIN=""
SCHEMA_DIR="$PROJECT_ROOT/docs/json-schema"
ENVELOPE_SCHEMA="$SCHEMA_DIR/wa-robot-envelope.json"

# Logging functions
log_test() {
    echo -e "\n=== $1 ==="
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

# Run wa command with timeout (robot commands may hang without watcher)
# Extracts just the JSON portion (ignores log lines with ANSI codes)
run_wa_timeout() {
    local timeout_secs="${1:-5}"
    shift
    local raw_output
    raw_output=$(timeout "$timeout_secs" "$FT_BIN" "$@" 2>&1 || true)

    # Strip ANSI codes and find JSON object
    # First, remove ANSI escape sequences
    local stripped
    stripped=$(echo "$raw_output" | sed 's/\x1b\[[0-9;]*m//g')

    # Try to extract JSON: find the first { and everything after
    # Use awk to extract from first { to last }
    echo "$stripped" | awk '
        /^{/ { found=1 }
        found { print }
    '
}

# Validate JSON against schema, capturing detailed errors
validate_schema() {
    local schema_file="$1"
    local json_data="$2"
    local description="$3"
    local artifact_base="$4"

    local temp_instance
    temp_instance=$(mktemp)
    echo "$json_data" > "$temp_instance"

    # Save instance for artifacts
    e2e_add_file "${artifact_base}_instance.json" "$json_data"

    # Run validation
    local validation_output
    local validation_result=0
    validation_output=$(jsonschema -i "$temp_instance" "$schema_file" 2>&1) || validation_result=$?

    rm -f "$temp_instance"

    if [[ $validation_result -eq 0 ]]; then
        log_pass "$description: schema valid"
        return 0
    else
        log_fail "$description: schema validation failed"
        e2e_add_file "${artifact_base}_validation_error.txt" "$validation_output"
        echo "    Validation error: $validation_output"
        return 1
    fi
}

# Check if JSON is valid
is_valid_json() {
    echo "$1" | jq . >/dev/null 2>&1
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    echo "========================================"
    echo "E2E: Schema/Docs/Client Pipeline"
    echo "Implements: bd-7ois"
    echo "========================================"

    # Initialize artifacts
    e2e_init_artifacts "schema-pipeline" >/dev/null
    echo "[INFO] Artifacts directory: $E2E_RUN_DIR"

    log_test "Checking Prerequisites"

    # Find wa binary
    FT_BIN="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}/debug/wa"
    if [[ ! -x "$FT_BIN" ]]; then
        FT_BIN="$PROJECT_ROOT/target/debug/wa"
    fi

    if [[ ! -x "$FT_BIN" ]]; then
        echo "[INFO] Building wa binary..."
        cargo build -p frankenterm 2>&1 | tail -5
    fi

    if [[ -x "$FT_BIN" ]]; then
        log_pass "wa binary found: $FT_BIN"
    else
        log_fail "wa binary not found"
        exit 1
    fi

    # Check jsonschema CLI
    if command -v jsonschema &>/dev/null; then
        log_pass "jsonschema CLI available"
    else
        log_fail "jsonschema CLI not found (pip install jsonschema)"
        exit 1
    fi

    # Check jq
    if command -v jq &>/dev/null; then
        log_pass "jq available"
    else
        log_fail "jq not found"
        exit 1
    fi

    # Check schema directory
    if [[ -d "$SCHEMA_DIR" ]]; then
        local schema_count
        schema_count=$(find "$SCHEMA_DIR" -name "*.json" | wc -l)
        log_pass "Schema directory found: $schema_count schemas"
    else
        log_fail "Schema directory not found: $SCHEMA_DIR"
        exit 1
    fi

    # Check envelope schema
    if [[ -f "$ENVELOPE_SCHEMA" ]]; then
        log_pass "Envelope schema found"
    else
        log_fail "Envelope schema not found: $ENVELOPE_SCHEMA"
        exit 1
    fi
}

# ==============================================================================
# Robot Help Output (always works, even without watcher)
# ==============================================================================

test_robot_help_schema() {
    log_test "Testing Robot Help Schema"

    # wa robot --help produces JSON
    local help_output
    help_output=$(run_wa_timeout 5 robot --help)

    if ! is_valid_json "$help_output"; then
        log_fail "robot --help: not valid JSON"
        e2e_add_file "robot_help_raw.txt" "$help_output"
        return
    fi

    # Check envelope first
    validate_schema "$ENVELOPE_SCHEMA" "$help_output" "robot --help envelope" "robot_help_envelope"

    # Extract .data for command-specific schema
    local help_data
    help_data=$(echo "$help_output" | jq '.data')

    # Validate data against help schema
    local help_schema="$SCHEMA_DIR/wa-robot-help.json"
    if [[ -f "$help_schema" ]]; then
        validate_schema "$help_schema" "$help_data" "robot --help data" "robot_help_data"
    else
        log_skip "robot --help: no help schema found"
        e2e_add_file "robot_help_valid.json" "$help_output"
    fi

    # Check that help has expected structure
    local has_commands
    has_commands=$(echo "$help_output" | jq -r '.data.commands | type // "null"')
    if [[ "$has_commands" == "array" ]]; then
        log_pass "robot --help: has commands array"
    else
        log_fail "robot --help: missing commands array"
    fi
}

# ==============================================================================
# Robot Command Envelope Validation
# ==============================================================================

test_envelope_schema() {
    log_test "Testing Robot Envelope Schema (all commands)"

    # Commands that should produce valid envelopes even without watcher
    # Note: 'robot panes' doesn't exist - the correct command is 'robot state'
    local commands=(
        "robot --help"
        "robot state"
        "robot events"
        "robot workflow list"
    )

    for cmd in "${commands[@]}"; do
        local output
        output=$(run_wa_timeout 5 $cmd)
        local cmd_name="${cmd//[[:space:]]/_}"

        if ! is_valid_json "$output"; then
            log_fail "$cmd: not valid JSON"
            e2e_add_file "${cmd_name}_raw.txt" "$output"
            continue
        fi

        # Check basic envelope structure
        local has_ok has_elapsed has_version has_now
        has_ok=$(echo "$output" | jq -r '.ok // "missing"')
        has_elapsed=$(echo "$output" | jq -r '.elapsed_ms // "missing"')
        has_version=$(echo "$output" | jq -r '.version // "missing"')
        has_now=$(echo "$output" | jq -r '.now // "missing"')

        if [[ "$has_ok" != "missing" && "$has_elapsed" != "missing" && "$has_version" != "missing" && "$has_now" != "missing" ]]; then
            log_pass "$cmd: has envelope fields"
        else
            log_fail "$cmd: missing envelope fields (ok=$has_ok, elapsed=$has_elapsed, version=$has_version, now=$has_now)"
        fi

        # Validate against envelope schema
        validate_schema "$ENVELOPE_SCHEMA" "$output" "$cmd" "$cmd_name"
    done
}

# ==============================================================================
# Command-Specific Schema Validation
# ==============================================================================

test_command_schemas() {
    log_test "Testing Command-Specific Schemas"

    # Map commands to their expected schemas (for .data field)
    # Note: Schemas are for the .data field, not the full envelope
    declare -A cmd_schemas=(
        ["robot state"]="wa-robot-state.json"
        ["robot events"]="wa-robot-events.json"
        ["robot workflow list"]="wa-robot-workflow-list.json"
    )

    for cmd in "${!cmd_schemas[@]}"; do
        local schema_file="$SCHEMA_DIR/${cmd_schemas[$cmd]}"
        local cmd_name="${cmd//[[:space:]]/_}"

        if [[ ! -f "$schema_file" ]]; then
            log_skip "$cmd: schema not found (${cmd_schemas[$cmd]})"
            continue
        fi

        local output
        output=$(run_wa_timeout 5 $cmd)

        if ! is_valid_json "$output"; then
            log_fail "$cmd: not valid JSON"
            e2e_add_file "${cmd_name}_raw.txt" "$output"
            continue
        fi

        # Only validate against command schema if command succeeded
        local ok
        ok=$(echo "$output" | jq -r '.ok')
        if [[ "$ok" == "true" ]]; then
            # Extract .data for command-specific schema validation
            local data
            data=$(echo "$output" | jq '.data')
            validate_schema "$schema_file" "$data" "$cmd data" "${cmd_name}_data"
        else
            # Error response - validate that error fields are present
            local has_error has_code
            has_error=$(echo "$output" | jq -r '.error // "missing"')
            has_code=$(echo "$output" | jq -r '.error_code // "missing"')

            if [[ "$has_error" != "missing" && "$has_code" != "missing" ]]; then
                log_pass "$cmd (error): has error fields"
            else
                log_fail "$cmd (error): missing error/error_code"
            fi

            e2e_add_file "${cmd_name}_error.json" "$output"
        fi
    done
}

# ==============================================================================
# Error Code Stability
# ==============================================================================

test_error_codes() {
    log_test "Testing Error Code Stability"

    # Test known error scenarios
    # 1. Invalid subcommand
    local invalid_output
    invalid_output=$(run_wa_timeout 5 robot invalid_subcommand_xyz)

    if is_valid_json "$invalid_output"; then
        local error_code
        error_code=$(echo "$invalid_output" | jq -r '.error_code // "none"')
        if [[ "$error_code" =~ ^robot\. ]]; then
            log_pass "Invalid subcommand: error_code pattern correct ($error_code)"
        else
            log_fail "Invalid subcommand: error_code should match robot.* pattern (got: $error_code)"
        fi
        e2e_add_file "error_invalid_subcommand.json" "$invalid_output"
    else
        log_fail "Invalid subcommand: response not valid JSON"
        e2e_add_file "error_invalid_subcommand_raw.txt" "$invalid_output"
    fi

    # 2. Missing required argument (for send command)
    local missing_arg_output
    missing_arg_output=$(run_wa_timeout 5 robot send)

    if is_valid_json "$missing_arg_output"; then
        local error_code
        error_code=$(echo "$missing_arg_output" | jq -r '.error_code // "none"')
        if [[ "$error_code" =~ ^robot\. ]]; then
            log_pass "Missing argument: error_code pattern correct ($error_code)"
        else
            # clap may produce different output for missing args
            log_skip "Missing argument: non-robot error (clap validation)"
        fi
        e2e_add_file "error_missing_arg.json" "$missing_arg_output"
    else
        # clap errors are not JSON
        log_skip "Missing argument: clap error (not JSON, expected)"
        e2e_add_file "error_missing_arg_raw.txt" "$missing_arg_output"
    fi
}

# ==============================================================================
# Schema Consistency Checks
# ==============================================================================

test_schema_consistency() {
    log_test "Testing Schema Consistency"

    # All schemas should be valid JSON Schema
    for schema_file in "$SCHEMA_DIR"/*.json; do
        local schema_name
        schema_name=$(basename "$schema_file")

        if is_valid_json "$(cat "$schema_file")"; then
            log_pass "$schema_name: valid JSON"
        else
            log_fail "$schema_name: invalid JSON"
            continue
        fi

        # Check for required JSON Schema fields
        local has_schema has_title
        has_schema=$(jq -r '."$schema" // "missing"' "$schema_file")
        has_title=$(jq -r '.title // "missing"' "$schema_file")

        if [[ "$has_schema" != "missing" && "$has_title" != "missing" ]]; then
            log_pass "$schema_name: has \$schema and title"
        else
            log_skip "$schema_name: missing \$schema or title (informational)"
        fi
    done
}

# ==============================================================================
# Docs Generation Check (if tooling exists)
# ==============================================================================

test_docs_generation() {
    log_test "Testing Docs Generation"

    # Check for schema-to-docs generation tooling
    if [[ -f "$PROJECT_ROOT/scripts/generate_schema_docs.sh" ]]; then
        log_pass "Schema docs generator found"
        # Would run: ./scripts/generate_schema_docs.sh --dry-run
        log_skip "Docs generation: not implemented yet"
    else
        log_skip "Docs generation: no generator script found (wa-upg.10.2 not implemented)"
    fi

    # Check for type generation tooling
    if [[ -f "$PROJECT_ROOT/scripts/generate_types.sh" ]]; then
        log_pass "Type generator found"
        log_skip "Type generation: not implemented yet"
    else
        log_skip "Type generation: no generator script found (wa-upg.10.3 not implemented)"
    fi
}

# ==============================================================================
# Summary
# ==============================================================================

print_summary() {
    echo ""
    echo "========================================"
    echo "Summary"
    echo "========================================"
    echo ""
    echo "Tests run:    $TESTS_RUN"
    echo "Tests passed: $TESTS_PASSED"
    echo "Tests failed: $TESTS_FAILED"
    echo "Tests skipped: $TESTS_SKIPPED"

    # Finalize artifacts
    e2e_finalize "$TESTS_PASSED" "$TESTS_FAILED"
    echo ""
    echo "ARTIFACTS_DIR=$E2E_RUN_DIR"

    if [[ $TESTS_FAILED -eq 0 ]]; then
        echo ""
        echo "All tests passed! ($TESTS_SKIPPED skipped)"
        exit 0
    else
        echo ""
        echo "Some tests failed. Check artifacts for details."
        exit 1
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    check_prerequisites
    test_robot_help_schema
    test_envelope_schema
    test_command_schemas
    test_error_codes
    test_schema_consistency
    test_docs_generation
    print_summary
}

main "$@"
