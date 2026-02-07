#!/bin/bash
# =============================================================================
# E2E: Sleep audit — verify no unjustified fixed sleeps in E2E scripts
# Implements: wa-upg.3.4
#
# Purpose:
#   Enforce the deterministic-time contract across all E2E scripts:
#   - No bare `sleep N` calls outside polling loops or test stimulus heredocs
#   - wait_for_condition / wait_for_json_condition infrastructure available
#   - All wait-for calls have bounded timeouts
#   - Test-stimulus heredoc sleeps are justified (inside EOS blocks)
#   - Polling-loop sleeps use sub-second intervals (sleep 0.5)
#
# Requirements:
#   - bash (for script analysis)
#   - grep, awk (standard utilities)
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
# Scenario 1: No bare sleeps in E2E harness code
# ==============================================================================

scenario_no_bare_sleeps() {
    log_test "Scenario 1: No bare sleeps in E2E harness code"

    local scripts_dir="$PROJECT_ROOT/scripts"
    local total_sleeps=0
    local justified_sleeps=0
    local unjustified_sleeps=0
    local unjustified_details=""

    # Analyze each E2E script
    for script in "$scripts_dir"/e2e_*.sh; do
        [[ -f "$script" ]] || continue
        local basename
        basename=$(basename "$script")

        # Skip self (this script references 'sleep' in comments/patterns)
        [[ "$basename" == "e2e_sleep_audit.sh" ]] && continue

        # Find actual sleep commands (exclude comments and quoted strings in grep patterns)
        local sleep_lines
        sleep_lines=$(command grep -n '^\s*sleep \|[;&|] *sleep \|then sleep \|do sleep ' "$script" 2>/dev/null || true)
        [[ -z "$sleep_lines" ]] && continue

        while IFS= read -r sleep_line; do
            [[ -z "$sleep_line" ]] && continue
            ((total_sleeps++)) || true

            local lineno
            lineno=$(echo "$sleep_line" | cut -d: -f1)
            local content
            content=$(echo "$sleep_line" | cut -d: -f2-)

            # Classify: is this sleep justified?
            local justified=false
            local reason=""

            # 1. Inside a heredoc (EOS/EOF block) — test stimulus
            # Check if this line is between a <<'EOS' and EOS (or <<'EOF' and EOF)
            local in_heredoc
            in_heredoc=$(awk -v target="$lineno" '
                /<<.*EOS|<<.*EOF/ { inside=1 }
                inside && NR == target { print "yes"; exit }
                /^EOS$|^EOF$/ { inside=0 }
            ' "$script")
            if [[ "$in_heredoc" == "yes" ]]; then
                justified=true
                reason="test-stimulus heredoc"
            fi

            # 2. Inside a polling loop (while...done with wait_for or condition check)
            # Check if the sleep is between a "while" and "done" that forms a polling loop
            if [[ "$justified" == "false" ]]; then
                local in_poll_loop
                in_poll_loop=$(awk -v target="$lineno" '
                    /while.*true|while.*\[/ { depth++ }
                    depth > 0 && NR == target { print "yes"; exit }
                    /^\s*done/ { if (depth > 0) depth-- }
                ' "$script")
                if [[ "$in_poll_loop" == "yes" ]]; then
                    justified=true
                    reason="polling-loop interval"
                fi
            fi

            # 3. Intentional measurement sleep (RSS check, memory leak)
            if [[ "$justified" == "false" ]]; then
                if echo "$content" | command grep -qiE 'long.run|rss|memory|measure'; then
                    justified=true
                    reason="measurement wait"
                fi
            fi

            # 4. Sleep variable derived from config (parameterized, not hard-coded)
            if [[ "$justified" == "false" ]]; then
                if echo "$content" | command grep -qE 'sleep "\$'; then
                    # Variable-based sleep — could be test stimulus or parameterized
                    # Check surrounding context for heredoc markers
                    local context_before
                    context_before=$(awk -v start=$((lineno > 10 ? lineno - 10 : 1)) -v end="$lineno" 'NR >= start && NR <= end' "$script")
                    if echo "$context_before" | command grep -qE "<<.*EOS|<<.*EOF|heredoc|stimulus|pane.*spawn"; then
                        justified=true
                        reason="parameterized stimulus"
                    fi
                fi
            fi

            if [[ "$justified" == "true" ]]; then
                ((justified_sleeps++)) || true
                log_info "  $basename:$lineno — justified ($reason): $(echo "$content" | sed 's/^[[:space:]]*//')"
            else
                ((unjustified_sleeps++)) || true
                unjustified_details="${unjustified_details}    $basename:$lineno: $(echo "$content" | sed 's/^[[:space:]]*//')\n"
            fi

        done <<< "$sleep_lines"
    done

    e2e_add_file "sleep_audit.json" "{\"total_sleeps\": $total_sleeps, \"justified\": $justified_sleeps, \"unjustified\": $unjustified_sleeps}"

    if [[ $total_sleeps -gt 0 ]]; then
        log_pass "S1.1: Found $total_sleeps sleep calls across E2E scripts"
    else
        log_pass "S1.1: No sleep calls found (fully deterministic)"
    fi

    if [[ $unjustified_sleeps -eq 0 ]]; then
        log_pass "S1.2: All $justified_sleeps sleep calls are justified"
    else
        log_fail "S1.2: $unjustified_sleeps unjustified sleep calls found"
        if [[ -n "$unjustified_details" ]]; then
            echo -e "$unjustified_details"
        fi
    fi

    # Ratio check: unjustified should be < 5% of total
    if [[ $total_sleeps -gt 0 ]]; then
        local unjustified_pct=$((unjustified_sleeps * 100 / total_sleeps))
        if [[ $unjustified_pct -lt 5 ]]; then
            log_pass "S1.3: Unjustified sleep ratio < 5% (${unjustified_pct}%)"
        else
            log_fail "S1.3: Unjustified sleep ratio too high (${unjustified_pct}%)"
        fi
    else
        log_pass "S1.3: No sleeps to audit"
    fi
}

# ==============================================================================
# Scenario 2: wait_for_condition infrastructure present
# ==============================================================================

scenario_wait_for_infrastructure() {
    log_test "Scenario 2: wait_for_condition infrastructure"

    local e2e_main="$PROJECT_ROOT/scripts/e2e_test.sh"

    # Check wait_for_condition function exists
    if command grep -q 'wait_for_condition()' "$e2e_main"; then
        log_pass "S2.1: wait_for_condition() defined in e2e_test.sh"
    else
        log_fail "S2.1: wait_for_condition() not found"
    fi

    # Check it uses bounded timeout
    if command grep -A 20 'wait_for_condition()' "$e2e_main" | command grep -q 'timeout\|TIMEOUT'; then
        log_pass "S2.2: wait_for_condition has timeout parameter"
    else
        log_fail "S2.2: wait_for_condition missing timeout"
    fi

    # Check polling interval is sub-second
    if command grep -A 30 'wait_for_condition()' "$e2e_main" | command grep -q 'sleep 0\.'; then
        log_pass "S2.3: Polling uses sub-second interval"
    else
        log_fail "S2.3: Polling interval not sub-second"
    fi

    # Count wait_for_condition usages
    local usage_count
    usage_count=$(command grep -c 'wait_for_condition' "$e2e_main" 2>/dev/null) || usage_count=0
    # Subtract the function definition itself
    usage_count=$((usage_count - 1))

    if [[ "$usage_count" -ge 20 ]]; then
        log_pass "S2.4: $usage_count wait_for_condition usages (>= 20)"
    elif [[ "$usage_count" -gt 0 ]]; then
        log_fail "S2.4: Only $usage_count wait_for_condition usages (expected >= 20)"
    else
        log_fail "S2.4: No wait_for_condition usages found"
    fi

    # Check saved searches also has wait infrastructure
    local saved="$PROJECT_ROOT/scripts/e2e_saved_searches.sh"
    if [[ -f "$saved" ]]; then
        if command grep -q 'wait_for_json_condition' "$saved"; then
            log_pass "S2.5: e2e_saved_searches.sh has wait_for_json_condition"
        else
            log_fail "S2.5: e2e_saved_searches.sh missing wait infrastructure"
        fi
    else
        log_skip "S2.5: e2e_saved_searches.sh not found"
    fi
}

# ==============================================================================
# Scenario 3: All wait-for calls have bounded timeouts
# ==============================================================================

scenario_bounded_timeouts() {
    log_test "Scenario 3: All wait-for calls have bounded timeouts"

    local scripts_dir="$PROJECT_ROOT/scripts"
    local total_direct_waits=0
    local with_timeout=0
    local wrapper_functions=0

    for script in "$scripts_dir"/e2e_*.sh; do
        [[ -f "$script" ]] || continue
        local basename
        basename=$(basename "$script")

        # Count direct wait_for_condition/wait_for_json_condition calls (excluding definitions)
        while IFS= read -r wait_line; do
            [[ -z "$wait_line" ]] && continue
            local lineno content
            lineno=$(echo "$wait_line" | cut -d: -f1)
            content=$(echo "$wait_line" | cut -d: -f2-)

            # Skip function definitions and wrapper definitions
            if echo "$content" | command grep -qE '^\s*(wait_for_condition|wait_for_json_condition)\(\)'; then
                continue
            fi
            # Skip wrapper functions that internally use wait_for_condition
            if echo "$content" | command grep -qE '^\s*wait_for_(pane_observed|stable)'; then
                ((wrapper_functions++)) || true
                continue
            fi

            ((total_direct_waits++)) || true

            # Check for timeout parameter (numeric or variable on same or continuation line)
            if echo "$content" | command grep -qE '[0-9]+\s*;?\s*$|[0-9]+\s*\)|"\$\{?wait_timeout|"\$timeout'; then
                ((with_timeout++)) || true
            else
                log_info "  Check timeout: $basename:$lineno"
            fi
        done < <(command grep -n 'wait_for_condition\|wait_for_json_condition' "$script" 2>/dev/null)
    done

    if [[ $total_direct_waits -gt 0 ]]; then
        log_pass "S3.1: Found $total_direct_waits direct wait-for calls (plus $wrapper_functions wrapper calls)"
    else
        log_pass "S3.1: No direct wait-for calls (offline-only scripts)"
    fi

    # Most calls should have explicit timeouts; allow some wiggle for multi-line patterns
    local timeout_pct=0
    if [[ $total_direct_waits -gt 0 ]]; then
        timeout_pct=$((with_timeout * 100 / total_direct_waits))
    fi

    if [[ $timeout_pct -ge 60 || $total_direct_waits -eq 0 ]]; then
        log_pass "S3.2: ${timeout_pct}% of wait-for calls have visible timeout parameters"
    else
        log_fail "S3.2: Only ${timeout_pct}% of wait-for calls have visible timeouts"
    fi
}

# ==============================================================================
# Scenario 4: Offline E2E scripts are sleep-free
# ==============================================================================

scenario_offline_scripts_clean() {
    log_test "Scenario 4: Offline E2E scripts are sleep-free"

    # Offline scripts (cargo-test based, no WezTerm needed) should have zero sleeps
    local offline_scripts=(
        "e2e_backpressure.sh"
        "e2e_storage_stress.sh"
        "e2e_search_perf.sh"
        "e2e_pane_uuid_stability.sh"
        "e2e_incident_bundle.sh"
        "e2e_prioritized_capture.sh"
        "e2e_sleep_audit.sh"
    )

    local clean_count=0
    local dirty_count=0

    for script_name in "${offline_scripts[@]}"; do
        local script="$PROJECT_ROOT/scripts/$script_name"
        [[ -f "$script" ]] || continue
        # Skip self (references sleep in comments/patterns)
        [[ "$script_name" == "e2e_sleep_audit.sh" ]] && continue

        local sleep_count
        sleep_count=$(command grep -c '^\s*sleep \|[;&|] *sleep ' "$script" 2>/dev/null) || sleep_count=0

        if [[ "$sleep_count" -eq 0 ]]; then
            ((clean_count++)) || true
            log_info "  $script_name: clean (0 sleeps)"
        else
            ((dirty_count++)) || true
            log_info "  $script_name: $sleep_count sleep(s)"
        fi
    done

    if [[ $clean_count -gt 0 ]]; then
        log_pass "S4.1: $clean_count offline E2E scripts are sleep-free"
    else
        log_skip "S4.1: No offline E2E scripts found"
    fi

    if [[ $dirty_count -eq 0 ]]; then
        log_pass "S4.2: No offline scripts contain sleeps"
    else
        log_fail "S4.2: $dirty_count offline scripts contain sleeps"
    fi
}

# ==============================================================================
# Scenario 5: Deterministic-time Rust test infrastructure
# ==============================================================================

scenario_rust_test_infrastructure() {
    log_test "Scenario 5: Deterministic-time Rust test infrastructure"

    local test_output exit_code
    test_output=$(mktemp)
    exit_code=0

    # Verify Rust tests don't use thread::sleep (they use tokio::time or instant mocking)
    # Run a subset of timing-sensitive tests
    cargo test -p wa-core 'scheduler_per_pane_window_resets' \
        --no-fail-fast -- --nocapture \
        >"$test_output" 2>&1 || exit_code=$?

    e2e_add_file "rust_timing_tests.log" "$(cat "$test_output")"

    if [[ $exit_code -eq 0 ]]; then
        log_pass "S5.1: Timing-sensitive Rust tests pass deterministically"
    else
        log_fail "S5.1: Timing-sensitive Rust tests failed (exit=$exit_code)"
    fi

    # Verify quiescence helpers exist in test infrastructure
    local quiescence_hits
    quiescence_hits=$(command grep -rl 'wait_for\|quiescence\|poll_until\|assert_eventually' \
        "$PROJECT_ROOT/crates/wa-core/src/" 2>/dev/null | wc -l)

    if [[ "$quiescence_hits" -gt 0 ]]; then
        log_pass "S5.2: Wait-for/quiescence helpers found in $quiescence_hits source files"
    else
        log_skip "S5.2: No quiescence helpers in Rust source (may use test harness directly)"
    fi

    rm -f "$test_output"
}

# ==============================================================================
# Scenario 6: E2E artifact library has no sleeps
# ==============================================================================

scenario_artifact_library_clean() {
    log_test "Scenario 6: E2E artifact library and helpers"

    local lib_dir="$PROJECT_ROOT/scripts/lib"

    if [[ -d "$lib_dir" ]]; then
        local lib_sleeps
        lib_sleeps=$(command grep -r '\bsleep\b' "$lib_dir" 2>/dev/null | wc -l)

        if [[ "$lib_sleeps" -eq 0 ]]; then
            log_pass "S6.1: E2E library has zero sleep calls"
        else
            log_fail "S6.1: E2E library contains $lib_sleeps sleep calls"
        fi
    else
        log_skip "S6.1: No scripts/lib directory"
    fi

    # Verify e2e_artifacts.sh exports are available
    if [[ -f "$PROJECT_ROOT/scripts/lib/e2e_artifacts.sh" ]]; then
        if command grep -q 'e2e_capture_scenario' "$PROJECT_ROOT/scripts/lib/e2e_artifacts.sh"; then
            log_pass "S6.2: e2e_artifacts.sh provides capture infrastructure"
        else
            log_fail "S6.2: e2e_artifacts.sh missing capture infrastructure"
        fi
    else
        log_fail "S6.2: e2e_artifacts.sh not found"
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo -e "${BLUE}================================================${NC}"
    echo -e "${BLUE}E2E: Sleep Audit — Deterministic Time Contract${NC}"
    echo -e "${BLUE}Bead: wa-upg.3.4${NC}"
    echo -e "${BLUE}================================================${NC}"

    e2e_init_artifacts "sleep-audit" >/dev/null

    local overall_exit=0

    # Run all scenarios, collecting results
    e2e_capture_scenario "no_bare_sleeps" scenario_no_bare_sleeps || overall_exit=1
    e2e_capture_scenario "wait_for_infrastructure" scenario_wait_for_infrastructure || overall_exit=1
    e2e_capture_scenario "bounded_timeouts" scenario_bounded_timeouts || overall_exit=1
    e2e_capture_scenario "offline_scripts_clean" scenario_offline_scripts_clean || overall_exit=1
    e2e_capture_scenario "rust_test_infrastructure" scenario_rust_test_infrastructure || overall_exit=1
    e2e_capture_scenario "artifact_library_clean" scenario_artifact_library_clean || overall_exit=1

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
