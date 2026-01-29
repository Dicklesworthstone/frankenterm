#!/bin/bash
# =============================================================================
# E2E: rules list/test + pack linter (fixture-first drift)
# Implements: wa-4vx.10.24
#
# Purpose:
#   Validate the rules toolchain end-to-end:
#   - wa robot rules list returns stable rule IDs + metadata
#   - wa robot rules test matches known fixtures correctly
#   - Negative fixtures produce no matches
#   - Output schemas are stable and parseable
#
# Requirements:
#   - wa binary built (cargo build -p wa)
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
WA_BIN=""
VERBOSE=false
CORPUS_DIR=""

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

# Run wa robot command, extracting JSON from output (strips log lines)
run_robot() {
    local raw_output
    raw_output=$("$WA_BIN" "$@" 2>/dev/null) || true
    # Extract JSON (first { to last })
    echo "$raw_output" | awk '/^{/{found=1} found{print}'
}

# Assert JSON field equals expected value
assert_json_eq() {
    local json="$1"
    local jq_expr="$2"
    local expected="$3"
    local description="$4"

    local actual
    actual=$(echo "$json" | jq -r "$jq_expr" 2>/dev/null || echo "JQ_ERROR")

    if [[ "$actual" == "$expected" ]]; then
        log_pass "$description"
    else
        log_fail "$description (expected='$expected', got='$actual')"
        log_info "JSON (first 300 chars): ${json:0:300}"
    fi
}

# Assert JSON field is greater than value
assert_json_gt() {
    local json="$1"
    local jq_expr="$2"
    local min_val="$3"
    local description="$4"

    local actual
    actual=$(echo "$json" | jq -r "$jq_expr" 2>/dev/null || echo "0")

    if [[ "$actual" -gt "$min_val" ]]; then
        log_pass "$description (got $actual)"
    else
        log_fail "$description (expected >$min_val, got $actual)"
    fi
}

# Assert JSON output contains a field
assert_json_has() {
    local json="$1"
    local jq_expr="$2"
    local description="$3"

    local result
    result=$(echo "$json" | jq -e "$jq_expr" 2>/dev/null)

    if [[ $? -eq 0 && "$result" != "null" ]]; then
        log_pass "$description"
    else
        log_fail "$description (field missing or null)"
    fi
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_test "Prerequisites"

    if [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/debug/wa"
    elif [[ -x "$PROJECT_ROOT/target/release/wa" ]]; then
        WA_BIN="$PROJECT_ROOT/target/release/wa"
    else
        echo -e "${RED}ERROR:${NC} wa binary not found. Run: cargo build -p wa" >&2
        exit 5
    fi
    log_pass "wa binary found: $WA_BIN"

    if ! command -v jq &>/dev/null; then
        echo -e "${RED}ERROR:${NC} jq not found. Install: sudo apt install jq" >&2
        exit 5
    fi
    log_pass "jq available"

    CORPUS_DIR="$PROJECT_ROOT/crates/wa-core/tests/corpus"
    if [[ -d "$CORPUS_DIR" ]]; then
        log_pass "corpus directory found: $CORPUS_DIR"
    else
        echo -e "${RED}ERROR:${NC} corpus directory not found at $CORPUS_DIR" >&2
        exit 5
    fi
}

# ==============================================================================
# Scenario 1: wa robot rules list — schema + stability
# ==============================================================================

test_rules_list() {
    log_test "Scenario 1: wa robot rules list"

    local json
    json=$(run_robot robot rules list)

    # 1.1: Output is valid JSON with ok=true
    assert_json_eq "$json" '.ok' 'true' "1.1: rules list returns ok=true"

    # 1.2: Contains rules array
    assert_json_has "$json" '.data.rules' "1.2: data.rules array exists"

    # 1.3: At least 26 rules (current count is 28)
    assert_json_gt "$json" '.data.rules | length' 25 "1.3: at least 26 rules present"

    # 1.4: Each rule has required fields
    local missing_fields
    missing_fields=$(echo "$json" | jq '[.data.rules[] | select(.id == null or .agent_type == null or .event_type == null or .severity == null)] | length' 2>/dev/null || echo "ERROR")
    if [[ "$missing_fields" == "0" ]]; then
        log_pass "1.4: all rules have id, agent_type, event_type, severity"
    else
        log_fail "1.4: $missing_fields rules missing required fields"
    fi

    # 1.5: Core packs present
    local packs
    packs=$(echo "$json" | jq -r '[.data.rules[].agent_type] | unique | sort | join(",")' 2>/dev/null)
    for pack in codex claude_code gemini wezterm; do
        if echo "$packs" | grep -qF "$pack"; then
            log_pass "1.5.$pack: pack '$pack' present"
        else
            log_fail "1.5.$pack: pack '$pack' missing (found: $packs)"
        fi
    done

    # 1.6: Rule IDs are unique
    local total_rules unique_ids
    total_rules=$(echo "$json" | jq '.data.rules | length' 2>/dev/null || echo "0")
    unique_ids=$(echo "$json" | jq '[.data.rules[].id] | unique | length' 2>/dev/null || echo "0")
    if [[ "$total_rules" == "$unique_ids" ]]; then
        log_pass "1.6: all rule IDs are unique ($total_rules rules)"
    else
        log_fail "1.6: duplicate rule IDs ($unique_ids unique out of $total_rules)"
    fi

    # 1.7: Output is deterministic (run twice, compare)
    local json2
    json2=$(run_robot robot rules list)
    local ids1 ids2
    ids1=$(echo "$json" | jq -r '[.data.rules[].id] | sort | join(",")' 2>/dev/null)
    ids2=$(echo "$json2" | jq -r '[.data.rules[].id] | sort | join(",")' 2>/dev/null)
    if [[ "$ids1" == "$ids2" ]]; then
        log_pass "1.7: rule IDs are deterministic across runs"
    else
        log_fail "1.7: rule IDs differ between runs"
    fi

    # Save artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/rules_list"
        mkdir -p "$scenario_dir"
        echo "$json" | jq . > "$scenario_dir/rules_list.json" 2>/dev/null || echo "$json" > "$scenario_dir/rules_list.json"
    fi
}

# ==============================================================================
# Scenario 2: wa robot rules test — known fixtures
# ==============================================================================

test_rules_match_fixtures() {
    log_test "Scenario 2: wa robot rules test — fixture matching"

    # Test known fixtures that should trigger specific rules
    local -A fixtures=(
        ["codex.auth.device_code_prompt"]="Enter this one-time code: ABCD-12345"
        ["codex.usage.reached"]="You've hit your usage limit. Please try again at 2026-01-20 12:34 UTC."
        ["claude_code.compaction"]="Auto-compact: context compacted 12,345 tokens to 3,210"
        ["claude_code.error.timeout"]="Request timed out after 30 seconds"
        ["claude_code.session.cost_summary"]="Session cost: \$2.50"
        ["gemini.usage.reached"]="Usage limit reached for all Pro models"
        ["wezterm.mux.connection_lost"]="mux server is unavailable"
    )

    local fixture_count=0
    for expected_rule in "${!fixtures[@]}"; do
        local input="${fixtures[$expected_rule]}"
        local json
        json=$(run_robot robot rules test "$input")
        fixture_count=$((fixture_count + 1))

        # Check ok=true
        local ok
        ok=$(echo "$json" | jq -r '.ok' 2>/dev/null)
        if [[ "$ok" != "true" ]]; then
            log_fail "2.$fixture_count: rules test failed for '$expected_rule' (ok != true)"
            continue
        fi

        # Check match_count > 0
        local match_count
        match_count=$(echo "$json" | jq -r '.data.match_count' 2>/dev/null || echo "0")
        if [[ "$match_count" -gt 0 ]]; then
            log_pass "2.$fixture_count: '$expected_rule' detected ($match_count matches)"
        else
            log_fail "2.$fixture_count: '$expected_rule' NOT detected (0 matches)"
            log_info "Input: $input"
            log_info "JSON: ${json:0:300}"
            continue
        fi

        # Check that the expected rule_id is in matches
        local found_rule
        found_rule=$(echo "$json" | jq -r ".data.matches[] | select(.rule_id == \"$expected_rule\") | .rule_id" 2>/dev/null)
        if [[ "$found_rule" == "$expected_rule" ]]; then
            log_pass "2.$fixture_count: correct rule_id '$expected_rule' in matches"
        else
            local actual_rules
            actual_rules=$(echo "$json" | jq -r '[.data.matches[].rule_id] | join(", ")' 2>/dev/null)
            log_fail "2.$fixture_count: expected rule '$expected_rule' but got: $actual_rules"
        fi
    done

    # Save artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/fixture_matching"
        mkdir -p "$scenario_dir"
    fi
}

# ==============================================================================
# Scenario 3: wa robot rules test — extraction
# ==============================================================================

test_rules_extraction() {
    log_test "Scenario 3: wa robot rules test — field extraction"

    # Test that extracted fields are correct
    local json

    # 3.1: Codex device code extraction
    json=$(run_robot robot rules test "Enter this one-time code: ABCD-12345")
    local code
    code=$(echo "$json" | jq -r '.data.matches[0].extracted.code // empty' 2>/dev/null)
    if [[ "$code" == "ABCD-12345" ]]; then
        log_pass "3.1: extracted device code = 'ABCD-12345'"
    else
        log_fail "3.1: expected code='ABCD-12345', got='$code'"
    fi

    # 3.2: Claude timeout duration extraction
    json=$(run_robot robot rules test "Request timed out after 30 seconds")
    local duration
    duration=$(echo "$json" | jq -r '.data.matches[0].extracted.duration // empty' 2>/dev/null)
    if [[ "$duration" == "30" ]]; then
        log_pass "3.2: extracted timeout duration = '30'"
    else
        log_fail "3.2: expected duration='30', got='$duration'"
    fi

    # 3.3: Claude cost extraction
    json=$(run_robot robot rules test 'Session cost: $2.50')
    local cost
    cost=$(echo "$json" | jq -r '.data.matches[0].extracted.cost // empty' 2>/dev/null)
    if [[ "$cost" == "2.50" ]]; then
        log_pass "3.3: extracted session cost = '2.50'"
    else
        log_fail "3.3: expected cost='2.50', got='$cost'"
    fi

    # 3.4: Codex usage reached reset time
    json=$(run_robot robot rules test "You've hit your usage limit. Please try again at 2026-01-20 12:34 UTC.")
    local reset_time
    reset_time=$(echo "$json" | jq -r '.data.matches[0].extracted.reset_time // empty' 2>/dev/null)
    if [[ -n "$reset_time" ]]; then
        log_pass "3.4: extracted reset_time = '$reset_time'"
    else
        log_fail "3.4: reset_time not extracted"
    fi
}

# ==============================================================================
# Scenario 4: wa robot rules test — negative fixtures
# ==============================================================================

test_rules_negative() {
    log_test "Scenario 4: wa robot rules test — negative fixtures (no false positives)"

    local -a negatives=(
        "Hello, world!"
        "This is a normal terminal line with no patterns."
        "$ ls -la"
        "total 64"
        "drwxrwxr-x 5 user user 4096 Jan 28 12:00 ."
        "The quick brown fox jumps over the lazy dog."
        "git commit -m 'update readme'"
        "npm install --save-dev typescript"
    )

    local neg_count=0
    for input in "${negatives[@]}"; do
        neg_count=$((neg_count + 1))
        local json
        json=$(run_robot robot rules test "$input")

        local match_count
        match_count=$(echo "$json" | jq -r '.data.match_count' 2>/dev/null || echo "ERROR")

        if [[ "$match_count" == "0" ]]; then
            log_pass "4.$neg_count: no false positive for '${input:0:40}...'"
        else
            local rules
            rules=$(echo "$json" | jq -r '[.data.matches[].rule_id] | join(", ")' 2>/dev/null)
            log_fail "4.$neg_count: false positive! Matched '$rules' for: '${input:0:40}...'"
        fi
    done
}

# ==============================================================================
# Scenario 5: Output schema validation
# ==============================================================================

test_schema_stability() {
    log_test "Scenario 5: Output schema validation"

    # 5.1: rules list schema
    local list_json
    list_json=$(run_robot robot rules list)

    # Must have: ok, data, data.rules (array)
    local schema_ok=true
    for field in '.ok' '.data' '.data.rules'; do
        if ! echo "$list_json" | jq -e "$field" &>/dev/null; then
            log_fail "5.1: rules list missing field: $field"
            schema_ok=false
        fi
    done
    if [[ "$schema_ok" == "true" ]]; then
        log_pass "5.1: rules list schema valid (ok, data, data.rules)"
    fi

    # 5.2: rules test schema
    local test_json
    test_json=$(run_robot robot rules test "Enter this one-time code: ABCD-12345")

    schema_ok=true
    for field in '.ok' '.data' '.data.text_length' '.data.match_count' '.data.matches' '.elapsed_ms' '.version'; do
        if ! echo "$test_json" | jq -e "$field" &>/dev/null; then
            log_fail "5.2: rules test missing field: $field"
            schema_ok=false
        fi
    done
    if [[ "$schema_ok" == "true" ]]; then
        log_pass "5.2: rules test schema valid (ok, data, text_length, match_count, matches, elapsed_ms, version)"
    fi

    # 5.3: Match entry schema
    local match_fields
    match_fields=$(echo "$test_json" | jq '.data.matches[0] | keys | sort | join(",")' 2>/dev/null)
    local required_match_fields="agent_type,confidence,event_type,extracted,matched_text,rule_id,severity"
    # Check each required field
    local all_present=true
    for field in rule_id agent_type event_type severity; do
        if ! echo "$test_json" | jq -e ".data.matches[0].$field" &>/dev/null; then
            log_fail "5.3: match entry missing required field: $field"
            all_present=false
        fi
    done
    if [[ "$all_present" == "true" ]]; then
        log_pass "5.3: match entry has all required fields (rule_id, agent_type, event_type, severity)"
    fi

    # Save artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/schema_validation"
        mkdir -p "$scenario_dir"
        echo "$list_json" | jq . > "$scenario_dir/rules_list_schema.json" 2>/dev/null
        echo "$test_json" | jq . > "$scenario_dir/rules_test_schema.json" 2>/dev/null
    fi
}

# ==============================================================================
# Scenario 6: Corpus fixture coverage
# ==============================================================================

test_corpus_coverage() {
    log_test "Scenario 6: Corpus fixture coverage"

    # Check that corpus files exist for each agent type
    local -a agent_types=("codex" "claude_code" "gemini" "wezterm")
    local total_fixtures=0

    for agent in "${agent_types[@]}"; do
        local agent_dir="$CORPUS_DIR/$agent"
        if [[ -d "$agent_dir" ]]; then
            local txt_count
            txt_count=$(find "$agent_dir" -name "*.txt" -type f | wc -l)
            local json_count
            json_count=$(find "$agent_dir" -name "*.expect.json" -type f | wc -l)
            total_fixtures=$((total_fixtures + txt_count))

            if [[ "$txt_count" -gt 0 ]]; then
                log_pass "6.$agent: $txt_count fixtures, $json_count expected outputs"
            else
                log_fail "6.$agent: no .txt fixtures found in $agent_dir"
            fi

            # Each .txt should have a matching .expect.json
            while IFS= read -r txt_file; do
                local base
                base=$(basename "$txt_file" .txt)
                local expect_file="$agent_dir/${base}.expect.json"
                if [[ ! -f "$expect_file" ]]; then
                    log_fail "6.$agent: missing expected output: ${base}.expect.json"
                fi
            done < <(find "$agent_dir" -name "*.txt" -type f 2>/dev/null)
        else
            log_fail "6.$agent: corpus directory missing: $agent_dir"
        fi
    done

    # Overall fixture count
    if [[ "$total_fixtures" -ge 20 ]]; then
        log_pass "6.total: $total_fixtures corpus fixtures (>= 20 minimum)"
    else
        log_fail "6.total: only $total_fixtures corpus fixtures (need >= 20)"
    fi

    # Save artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/corpus_coverage"
        mkdir -p "$scenario_dir"
        find "$CORPUS_DIR" -name "*.txt" -type f | sort > "$scenario_dir/fixture_list.txt"
    fi
}

# ==============================================================================
# Scenario 7: Pack linter checks
# ==============================================================================

test_pack_linter() {
    log_test "Scenario 7: Pack linter (rule quality checks)"

    local json
    json=$(run_robot robot rules list)

    # 7.1: No rules with empty ID
    local empty_ids
    empty_ids=$(echo "$json" | jq '[.data.rules[] | select(.id == "" or .id == null)] | length' 2>/dev/null || echo "ERROR")
    if [[ "$empty_ids" == "0" ]]; then
        log_pass "7.1: no rules with empty/null IDs"
    else
        log_fail "7.1: $empty_ids rules with empty/null IDs"
    fi

    # 7.2: Rule IDs follow naming convention (agent_type.*)
    local bad_ids
    bad_ids=$(echo "$json" | jq '[.data.rules[] | select(.id | test("^(codex|claude_code|gemini|wezterm)\\.") | not)] | length' 2>/dev/null || echo "ERROR")
    if [[ "$bad_ids" == "0" ]]; then
        log_pass "7.2: all rule IDs follow pack.name convention"
    else
        log_fail "7.2: $bad_ids rules don't follow pack.name convention"
        log_info "$(echo "$json" | jq -r '[.data.rules[] | select(.id | test("^(codex|claude_code|gemini|wezterm)\\.") | not) | .id] | join(", ")' 2>/dev/null)"
    fi

    # 7.3: All rules have valid severity
    local bad_severity
    bad_severity=$(echo "$json" | jq '[.data.rules[] | select(.severity | IN("info","warning","critical","error") | not)] | length' 2>/dev/null || echo "ERROR")
    if [[ "$bad_severity" == "0" ]]; then
        log_pass "7.3: all rules have valid severity (info/warning/critical/error)"
    else
        log_fail "7.3: $bad_severity rules with invalid severity"
    fi

    # 7.4: Each agent type has at least 1 rule with anchors
    for agent in codex claude_code gemini wezterm; do
        local has_anchor
        has_anchor=$(echo "$json" | jq "[.data.rules[] | select(.agent_type == \"$agent\" and .anchor_count > 0)] | length" 2>/dev/null || echo "0")
        if [[ "$has_anchor" -gt 0 ]]; then
            log_pass "7.4.$agent: has $has_anchor rules with anchors"
        else
            log_fail "7.4.$agent: no rules with anchors"
        fi
    done

    # 7.5: At least some rules have regex extraction
    local regex_count
    regex_count=$(echo "$json" | jq '[.data.rules[] | select(.has_regex == true)] | length' 2>/dev/null || echo "0")
    if [[ "$regex_count" -gt 5 ]]; then
        log_pass "7.5: $regex_count rules have regex extraction"
    else
        log_fail "7.5: only $regex_count rules have regex (expected > 5)"
    fi

    # Save linter report artifact
    if [[ -n "${E2E_RUN_DIR:-}" ]]; then
        local scenario_dir="$E2E_SCENARIOS_DIR/pack_linter"
        mkdir -p "$scenario_dir"

        # Generate linter report
        cat > "$scenario_dir/linter_report.json" <<EOF
{
  "timestamp": "$(date -u +"%Y-%m-%dT%H:%M:%SZ")",
  "checks": {
    "empty_ids": $empty_ids,
    "bad_naming": $bad_ids,
    "bad_severity": $bad_severity,
    "regex_rules": $regex_count
  },
  "total_rules": $(echo "$json" | jq '.data.rules | length' 2>/dev/null || echo 0),
  "packs_found": $(echo "$json" | jq '[.data.rules[].agent_type] | unique | length' 2>/dev/null || echo 0)
}
EOF
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo "================================================================"
    echo "  E2E: rules list/test + pack linter (wa-4vx.10.24)"
    echo "================================================================"
    echo ""

    check_prerequisites

    # Initialize artifact collection
    e2e_init_artifacts "e2e-rules-test" > /dev/null

    # Run all scenarios
    test_rules_list
    test_rules_match_fixtures
    test_rules_extraction
    test_rules_negative
    test_schema_stability
    test_corpus_coverage
    test_pack_linter

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
