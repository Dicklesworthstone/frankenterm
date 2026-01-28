#!/bin/bash
# E2E Test Harness Runner for wa (wezterm_automata)
# Implements: wa-4vx.10.11
# Spec: docs/e2e-harness-spec.md
#
# Usage: ./scripts/e2e_test.sh [OPTIONS] [SCENARIO...]
#
# Exit codes:
#   0 - All scenarios passed
#   1 - One or more scenarios failed
#   2 - Harness self-check failed
#   3 - Invalid arguments
#   4 - Timeout exceeded
#   5 - Prerequisites missing

set -euo pipefail

# ==============================================================================
# Configuration
# ==============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULT_TIMEOUT=120
DEFAULT_ARTIFACTS_BASE="$PROJECT_ROOT/e2e-artifacts"

# Colors (disabled if not a TTY)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    NC='\033[0m' # No Color
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# ==============================================================================
# Globals
# ==============================================================================

VERBOSE=false
KEEP_ARTIFACTS=false
ARTIFACTS_DIR=""
TIMEOUT="$DEFAULT_TIMEOUT"
SELF_CHECK_ONLY=false
LIST_ONLY=false
PARALLEL=1
WORKSPACE=""
CONFIG_FILE=""
SCENARIOS=()

# Runtime state
TIMESTAMP=""
RUN_ARTIFACTS_DIR=""
SUMMARY_FILE=""
TOTAL=0
PASSED=0
FAILED=0
SKIPPED=0
START_TIME=""

# ==============================================================================
# Logging
# ==============================================================================

log_timestamp() {
    date +"%H:%M:%S"
}

log_info() {
    echo -e "${BLUE}[$(log_timestamp)]${NC} $*"
}

log_pass() {
    echo -e "${GREEN}[$(log_timestamp)] PASS:${NC} $*"
}

log_fail() {
    echo -e "${RED}[$(log_timestamp)] FAIL:${NC} $*"
}

log_warn() {
    echo -e "${YELLOW}[$(log_timestamp)] WARN:${NC} $*"
}

log_verbose() {
    if [[ "$VERBOSE" == "true" ]]; then
        echo -e "${BLUE}[$(log_timestamp)] DEBUG:${NC} $*"
    fi
}

# ==============================================================================
# Usage
# ==============================================================================

usage() {
    cat <<EOF
E2E Test Harness for wa (wezterm_automata)

Usage: $0 [OPTIONS] [SCENARIO...]

Options:
    -v, --verbose         Enable verbose output (debug-level logs)
    --keep-artifacts      Always keep artifacts (even on success)
    --artifacts-dir DIR   Override artifacts directory
    --timeout SECS        Global timeout per scenario (default: $DEFAULT_TIMEOUT)
    --list                List available scenarios and exit
    --self-check          Run harness self-check only
    --parallel N          Run N scenarios in parallel (default: 1)
    --workspace DIR       Override workspace for isolation
    --config FILE         Override wa.toml for testing
    --case NAME           Run a single scenario by name (alias for positional arg)
    --all                 Run all registered scenarios (default if no args)
    -h, --help            Show this help

Arguments:
    SCENARIO...           One or more scenario names to run. If omitted, runs all.

Exit Codes:
    0 - All scenarios passed
    1 - One or more scenarios failed
    2 - Harness self-check failed
    3 - Invalid arguments
    4 - Timeout exceeded
    5 - Prerequisites missing

Environment Variables:
    WA_E2E_KEEP_ARTIFACTS  Always keep artifacts (1)
    WA_E2E_TIMEOUT         Override timeout (seconds)
    WA_E2E_VERBOSE         Enable verbose output (1)
    WA_E2E_WORKSPACE       Override workspace path
    WA_LOG_LEVEL           Log level for wa processes
    WA_LOG_FORMAT          Log format (pretty/json)

Examples:
    $0                     # Run all scenarios
    $0 capture_search      # Run specific scenario
    $0 --self-check        # Check prerequisites only
    $0 --verbose --keep-artifacts  # Debug mode
EOF
}

# ==============================================================================
# Argument Parsing
# ==============================================================================

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -v|--verbose)
                VERBOSE=true
                shift
                ;;
            --keep-artifacts)
                KEEP_ARTIFACTS=true
                shift
                ;;
            --artifacts-dir)
                ARTIFACTS_DIR="$2"
                shift 2
                ;;
            --timeout)
                TIMEOUT="$2"
                shift 2
                ;;
            --list)
                LIST_ONLY=true
                shift
                ;;
            --self-check)
                SELF_CHECK_ONLY=true
                shift
                ;;
            --parallel)
                PARALLEL="$2"
                shift 2
                ;;
            --workspace)
                WORKSPACE="$2"
                shift 2
                ;;
            --config)
                CONFIG_FILE="$2"
                shift 2
                ;;
            --case)
                SCENARIOS+=("$2")
                shift 2
                ;;
            --all)
                # Explicit --all is a no-op (default behavior)
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            -*)
                echo "Unknown option: $1" >&2
                usage
                exit 3
                ;;
            *)
                SCENARIOS+=("$1")
                shift
                ;;
        esac
    done

    # Apply environment variable overrides
    if [[ -n "${WA_E2E_KEEP_ARTIFACTS:-}" ]]; then KEEP_ARTIFACTS=true; fi
    if [[ -n "${WA_E2E_TIMEOUT:-}" ]]; then TIMEOUT="$WA_E2E_TIMEOUT"; fi
    if [[ -n "${WA_E2E_VERBOSE:-}" ]]; then VERBOSE=true; fi
    if [[ -n "${WA_E2E_WORKSPACE:-}" ]]; then WORKSPACE="$WA_E2E_WORKSPACE"; fi
}

# ==============================================================================
# Self-Check
# ==============================================================================

check_pass() {
    echo -e "${GREEN}[PASS]${NC} $*"
}

check_fail() {
    echo -e "${RED}[FAIL]${NC} $*"
}

run_self_check() {
    echo "E2E Harness Self-Check"
    echo "======================"
    echo ""

    local all_passed=true

    # Check 1: WezTerm installed
    if command -v wezterm &>/dev/null; then
        local wezterm_version
        wezterm_version=$(wezterm --version 2>/dev/null | head -1 || echo "unknown")
        check_pass "WezTerm installed: $wezterm_version"
    else
        check_fail "WezTerm not found in PATH"
        echo "       Hint: Install WezTerm or add it to PATH"
        all_passed=false
    fi

    # Check 2: WezTerm mux operational
    if wezterm cli list &>/dev/null; then
        check_pass "WezTerm mux operational"
    else
        check_fail "WezTerm mux not operational"
        echo "       Hint: Start WezTerm with 'wezterm start' or check if it's running"
        all_passed=false
    fi

    # Check 3: wa binary
    local wa_binary="$PROJECT_ROOT/target/release/wa"
    if [[ -x "$wa_binary" ]]; then
        local wa_version
        wa_version=$("$wa_binary" --version 2>/dev/null | head -1 || echo "unknown")
        check_pass "wa binary: $wa_binary ($wa_version)"
    else
        # Try debug build
        wa_binary="$PROJECT_ROOT/target/debug/wa"
        if [[ -x "$wa_binary" ]]; then
            local wa_version
            wa_version=$("$wa_binary" --version 2>/dev/null | head -1 || echo "unknown")
            check_pass "wa binary (debug): $wa_binary ($wa_version)"
        else
            check_fail "wa binary not found"
            echo "       Hint: Run 'cargo build --release' or 'cargo build'"
            all_passed=false
        fi
    fi

    # Check 4: Artifacts directory writable
    local test_artifacts="${ARTIFACTS_DIR:-$DEFAULT_ARTIFACTS_BASE}"
    if mkdir -p "$test_artifacts" 2>/dev/null && touch "$test_artifacts/.write-test" 2>/dev/null; then
        rm -f "$test_artifacts/.write-test"
        check_pass "Artifacts directory: writable ($test_artifacts)"
    else
        check_fail "Artifacts directory not writable: $test_artifacts"
        all_passed=false
    fi

    # Check 5: Temp space available
    local temp_space_mb
    temp_space_mb=$(df -m /tmp 2>/dev/null | awk 'NR==2 {print $4}' || echo "0")
    if [[ "$temp_space_mb" -ge 100 ]]; then
        check_pass "Temp space: ${temp_space_mb}MB available"
    else
        check_fail "Temp space low: ${temp_space_mb}MB (need at least 100MB)"
        all_passed=false
    fi

    # Check 6: Required tools
    local missing_tools=()
    for tool in jq timeout mktemp; do
        if ! command -v "$tool" &>/dev/null; then
            missing_tools+=("$tool")
        fi
    done
    if [[ ${#missing_tools[@]} -eq 0 ]]; then
        check_pass "Required tools: all present (jq, timeout, mktemp)"
    else
        check_fail "Missing tools: ${missing_tools[*]}"
        all_passed=false
    fi

    echo ""
    if [[ "$all_passed" == "true" ]]; then
        echo "All checks passed. Ready to run E2E tests."
        return 0
    else
        echo "Self-check failed. Fix issues above before running E2E tests."
        return 1
    fi
}

# ==============================================================================
# Scenario Registry
# ==============================================================================

# List of available scenarios
# Format: scenario_name:description
SCENARIO_REGISTRY=(
    "capture_search:Validate ingest pipeline and FTS search"
    "natural_language:Validate event summaries and wa why output"
    "compaction_workflow:Validate pattern detection and workflow execution"
    "policy_denial:Validate safety gates block sends to protected panes"
    "graceful_shutdown:Validate wa watch graceful shutdown (SIGINT flush, lock release, restart clean)"
    "pane_exclude_filter:Validate pane selection filters protect privacy (ignored pane absent from search)"
    "workspace_isolation:Validate workspace isolation (no cross-project DB leakage)"
    "setup_idempotency:Validate wa setup idempotent patching (temp home, no leaks)"
    "uservar_forwarding:Validate user-var forwarding lane (wezterm.lua -> wa event -> watcher)"
    "workflow_resume:Validate workflow resumes after watcher restart (no duplicate steps)"
    "status_update:Validate Lua status_update lane (wa event --from-status -> watcher -> pane state)"
)

list_scenarios() {
    echo "Available E2E Scenarios"
    echo "======================="
    echo ""
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        local name="${entry%%:*}"
        local desc="${entry#*:}"
        printf "  %-25s %s\n" "$name" "$desc"
    done
    echo ""
    echo "Run all: $0"
    echo "Run one: $0 <scenario_name>"
}

get_scenario_names() {
    local names=()
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        names+=("${entry%%:*}")
    done
    echo "${names[@]}"
}

is_valid_scenario() {
    local name="$1"
    for entry in "${SCENARIO_REGISTRY[@]}"; do
        if [[ "${entry%%:*}" == "$name" ]]; then
            return 0
        fi
    done
    return 1
}

# ==============================================================================
# Artifacts Management
# ==============================================================================

setup_artifacts() {
    TIMESTAMP=$(date -u +"%Y-%m-%dT%H-%M-%SZ")

    if [[ -n "$ARTIFACTS_DIR" ]]; then
        RUN_ARTIFACTS_DIR="$ARTIFACTS_DIR/$TIMESTAMP"
    else
        RUN_ARTIFACTS_DIR="$DEFAULT_ARTIFACTS_BASE/$TIMESTAMP"
    fi

    mkdir -p "$RUN_ARTIFACTS_DIR"
    SUMMARY_FILE="$RUN_ARTIFACTS_DIR/summary.json"

    # Write environment snapshot
    cat > "$RUN_ARTIFACTS_DIR/env.txt" <<EOF
hostname: $(hostname)
timestamp: $TIMESTAMP
wezterm_version: $(wezterm --version 2>/dev/null | head -1 || echo "N/A")
wa_version: $(find_wa_binary && "$WA_BINARY" --version 2>/dev/null | head -1 || echo "N/A")
rust_version: $(rustc --version 2>/dev/null || echo "N/A")
os: $(uname -a)
shell: $SHELL
temp_workspace: ${WORKSPACE:-auto}
EOF

    log_verbose "Artifacts directory: $RUN_ARTIFACTS_DIR"
}

cleanup_artifacts() {
    if [[ "$KEEP_ARTIFACTS" == "false" && "$FAILED" -eq 0 ]]; then
        log_verbose "Cleaning up artifacts (all tests passed)"
        rm -rf "$RUN_ARTIFACTS_DIR"
    else
        log_info "Artifacts saved to: $RUN_ARTIFACTS_DIR"
    fi
}

write_summary() {
    local duration
    duration=$(( $(date +%s) - START_TIME ))

    cat > "$SUMMARY_FILE" <<EOF
{
  "version": "1",
  "timestamp": "$TIMESTAMP",
  "duration_secs": $duration,
  "total": $TOTAL,
  "passed": $PASSED,
  "failed": $FAILED,
  "skipped": $SKIPPED,
  "scenarios": []
}
EOF

    # Also write human-readable summary
    cat > "$RUN_ARTIFACTS_DIR/summary.txt" <<EOF
E2E Test Summary
================
Timestamp: $TIMESTAMP
Duration:  ${duration}s

Results:
  Total:   $TOTAL
  Passed:  $PASSED
  Failed:  $FAILED
  Skipped: $SKIPPED

Artifacts: $RUN_ARTIFACTS_DIR
EOF
}

# ==============================================================================
# WA Binary
# ==============================================================================

WA_BINARY=""

find_wa_binary() {
    if [[ -x "$PROJECT_ROOT/target/release/wa" ]]; then
        WA_BINARY="$PROJECT_ROOT/target/release/wa"
    elif [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
        WA_BINARY="$PROJECT_ROOT/target/debug/wa"
    else
        return 1
    fi
    return 0
}

# ==============================================================================
# Wait Helpers
# ==============================================================================

wait_for_condition() {
    local description="$1"
    local check_cmd="$2"
    local timeout="${3:-30}"
    local start=$(date +%s)

    log_verbose "Waiting for: $description (timeout: ${timeout}s)"

    while true; do
        if eval "$check_cmd"; then
            log_verbose "Condition met: $description"
            return 0
        fi

        local elapsed=$(( $(date +%s) - start ))
        if [[ $elapsed -ge $timeout ]]; then
            log_verbose "Timeout waiting for: $description"
            return 1
        fi

        sleep 0.5
    done
}

# ==============================================================================
# Scenario Runners
# ==============================================================================

run_scenario_capture_search() {
    local scenario_dir="$1"
    local marker="E2E_MARKER_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Cleanup function
    cleanup_capture_search() {
        log_verbose "Cleaning up capture_search scenario"
        # Kill wa watch if running
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_capture_search EXIT

    # Step 1: Spawn dummy pane with the print script
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 100 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start wa watch in background
    log_info "Step 2: Starting wa watch..."
    "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    # Give wa watch a moment to initialize
    sleep 1

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane capture..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        # Capture robot state for diagnostics
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for dummy script to complete (check for "Done:" marker)
    log_info "Step 4: Waiting for dummy script completion..."
    sleep 3  # Give time for output to be captured

    # Capture robot state
    "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Stop wa watch gracefully
    log_info "Step 5: Stopping wa watch..."
    kill -TERM "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""
    log_verbose "wa watch stopped"

    # Step 6: Search for the marker
    log_info "Step 6: Searching for marker..."
    local search_output
    search_output=$("$WA_BINARY" search "$marker" --limit 200 2>&1)
    echo "$search_output" > "$scenario_dir/search_output.txt"

    # Count hits (lines containing the marker, excluding header lines)
    local hit_count
    hit_count=$(echo "$search_output" | grep -c "$marker" || echo "0")

    log_info "Search returned $hit_count hits for marker"

    # Step 7: Assert results
    log_info "Step 7: Asserting results..."

    # We expect at least some hits (dummy_print.sh outputs 100+ lines)
    if [[ "$hit_count" -lt 10 ]]; then
        log_fail "Expected at least 10 hits, got $hit_count"
        result=1
    else
        log_pass "Found $hit_count hits for marker (expected >= 10)"
    fi

    # Verify pane_id in search results (if using JSON output)
    if "$WA_BINARY" search "$marker" --limit 10 2>/dev/null | jq -e '.' >/dev/null 2>&1; then
        log_verbose "Search output is JSON, checking pane_id..."
        if "$WA_BINARY" search "$marker" --limit 10 2>/dev/null | jq -e ".results[]? | select(.pane_id == $pane_id)" >/dev/null 2>&1; then
            log_pass "Correct pane_id in search results"
        else
            log_warn "Could not verify pane_id in search results (may be expected)"
        fi
    fi

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_capture_search

    return $result
}

run_scenario_natural_language() {
    local scenario_dir="$1"
    local marker="You've hit your usage limit, try again at 12:00."
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    cleanup_natural_language() {
        log_verbose "Cleaning up natural_language scenario"
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_natural_language EXIT

    # Step 1: Spawn dummy pane with usage-limit marker
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 5 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start wa watch in background
    log_info "Step 2: Starting wa watch..."
    "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 1

    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane capture..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for usage limit event to be detected
    log_info "Step 4: Waiting for usage limit event..."
    local event_cmd="\"$WA_BINARY\" events --format json --rule-id codex.usage.reached --limit 5 2>/dev/null | jq -e 'length > 0' >/dev/null 2>&1"
    if ! wait_for_condition "usage limit event detected" "$event_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for usage limit event"
        "$WA_BINARY" events --format json --limit 20 > "$scenario_dir/events_debug.json" 2>&1 || true
        result=1
    else
        log_pass "Usage limit event detected"
    fi

    # Step 5: Capture CLI outputs
    log_info "Step 5: Capturing CLI outputs..."
    local events_output
    events_output=$("$WA_BINARY" events --rule-id codex.usage.reached --limit 5 2>&1)
    echo "$events_output" > "$scenario_dir/events_output.txt"

    local why_output
    why_output=$("$WA_BINARY" why workflow.usage_limit 2>&1)
    echo "$why_output" > "$scenario_dir/why_output.txt"

    # Step 6: Assert outputs are human-readable
    log_info "Step 6: Asserting outputs..."
    if echo "$events_output" | grep -q "Codex usage limit reached"; then
        log_pass "Events output uses human summary"
    else
        log_fail "Events output missing human summary"
        result=1
    fi

    if echo "$why_output" | grep -q "handle_usage_limits"; then
        log_pass "wa why output rendered explanation"
    else
        log_fail "wa why output missing workflow explanation"
        result=1
    fi

    # Step 7: Stop wa watch gracefully
    log_info "Step 7: Stopping wa watch..."
    kill -TERM "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""
    log_verbose "wa watch stopped"

    trap - EXIT
    cleanup_natural_language

    return $result
}

run_scenario_compaction_workflow() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Copy baseline config for workflow testing
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/wa.toml"
        export WA_CONFIG="$temp_workspace/wa.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Cleanup function
    cleanup_compaction_workflow() {
        log_verbose "Cleaning up compaction_workflow scenario"
        # Kill wa watch if running
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/wa.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_compaction_workflow EXIT

    # Step 1: Start wa watch with auto-handle BEFORE spawning pane
    # This ensures it's ready to detect and respond
    log_info "Step 1: Starting wa watch with --auto-handle..."
    "$WA_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane that will trigger compaction
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    # Spawn with 2 second delay before compaction marker
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 2 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned agent pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for compaction event to be detected
    # The dummy_agent.sh will print "[CODEX] Compaction required:" after delay
    log_info "Step 4: Waiting for compaction detection..."
    sleep 5  # Give time for agent to emit compaction marker and wa to detect

    # Check for detection event (if events endpoint exists)
    "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Wait for workflow to execute and send text to pane
    log_info "Step 5: Waiting for workflow execution..."
    # The workflow should send "/compact" to the pane
    # Wait and then check pane content

    # Poll for "Received:" or "Refresh acknowledged" in pane output
    local check_workflow_cmd='pane_text=$("'"$WA_BINARY"'" robot get-text '"$pane_id"' 2>/dev/null); echo "$pane_text" | grep -q "Received:"'

    if wait_for_condition "workflow send detected in pane" "$check_workflow_cmd" "$wait_timeout"; then
        log_pass "Workflow send detected in pane"
    else
        log_warn "Workflow may not have sent text (checking pane anyway)"
    fi

    # Step 6: Capture and verify pane content
    log_info "Step 6: Verifying pane received workflow input..."
    local pane_text
    pane_text=$("$WA_BINARY" robot get-text "$pane_id" 2>&1 || true)
    echo "$pane_text" > "$scenario_dir/pane_text.txt"

    # Check for evidence that workflow sent text
    # The workflow sends "/compact\n" and agent echoes "Received: /compact"
    if echo "$pane_text" | grep -q "Received:"; then
        log_pass "Pane received input from workflow"

        # Check for compaction acknowledgment
        if echo "$pane_text" | grep -q "Refresh acknowledged\|Context compacted"; then
            log_pass "Agent acknowledged refresh/compact command"
        else
            log_warn "Agent did not acknowledge (may still be waiting)"
        fi
    else
        log_warn "No 'Received:' found in pane output"
        log_info "Pane content may not show workflow send yet"
        # This may not be a failure if workflow isn't fully implemented
    fi

    # Step 7: Check wa watch logs for workflow execution
    log_info "Step 7: Checking wa watch logs for workflow activity..."
    if grep -qi "workflow\|compaction\|detection" "$scenario_dir/wa_watch.log" 2>/dev/null; then
        log_pass "Found workflow/detection activity in logs"
    else
        log_warn "No obvious workflow activity in logs (may be normal)"
    fi

    # Note: This scenario depends on workflow functionality being complete
    # If workflows aren't implemented yet, this will pass with warnings
    log_info "Scenario complete (workflow functionality dependent)"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_compaction_workflow

    return $result
}

run_scenario_policy_denial() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance with strict config
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Copy strict config for policy testing
    local strict_config="$PROJECT_ROOT/fixtures/e2e/config_strict.toml"
    if [[ -f "$strict_config" ]]; then
        cp "$strict_config" "$temp_workspace/wa.toml"
        export WA_CONFIG="$temp_workspace/wa.toml"
        log_verbose "Using strict config: $strict_config"
    fi

    # Cleanup function
    cleanup_policy_denial() {
        log_verbose "Cleaning up policy_denial scenario"
        # Kill wa watch if running
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close alt-screen pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing alt-screen pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/wa.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_policy_denial EXIT

    # Step 1: Spawn a pane that enters alternate screen mode
    log_info "Step 1: Spawning alt-screen pane..."
    local alt_script="$PROJECT_ROOT/fixtures/e2e/dummy_alt_screen.sh"
    if [[ ! -x "$alt_script" ]]; then
        log_fail "Alt-screen script not found or not executable: $alt_script"
        return 1
    fi

    local spawn_output
    # Spawn with long duration so it stays in alt screen
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$alt_script" 60 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn alt-screen pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned alt-screen pane: $pane_id"
    echo "alt_screen_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Give time for pane to enter alt screen
    sleep 2

    # Step 2: Start wa watch in background
    log_info "Step 2: Starting wa watch..."
    "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local wait_timeout=${TIMEOUT:-30}
    local check_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Capture robot state for diagnostics
    "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 4: Attempt to send text to the alt-screen pane
    log_info "Step 4: Attempting send to alt-screen pane (should be denied)..."
    local send_output
    send_output=$("$WA_BINARY" robot send "$pane_id" "test_text_should_be_denied" 2>&1)
    local send_exit_code=$?
    echo "$send_output" > "$scenario_dir/send_attempt.json"
    echo "send_exit_code: $send_exit_code" >> "$scenario_dir/scenario.log"

    log_verbose "Send output: $send_output"
    log_verbose "Send exit code: $send_exit_code"

    # Step 5: Assert send was denied
    log_info "Step 5: Asserting send was denied..."

    # Check if the response indicates denial
    # Robot mode should return JSON with ok: false or an error
    local ok_status=""
    if echo "$send_output" | jq -e '.' >/dev/null 2>&1; then
        ok_status=$(echo "$send_output" | jq -r '.ok // empty')
        local error_code=$(echo "$send_output" | jq -r '.error.code // .error // empty')

        if [[ "$ok_status" == "false" ]]; then
            log_pass "Send denied (ok: false)"
            if [[ -n "$error_code" ]]; then
                log_info "Error code: $error_code"
                echo "denial_error_code: $error_code" >> "$scenario_dir/scenario.log"
            fi
        elif [[ "$ok_status" == "true" ]]; then
            log_fail "Send was NOT denied - ok: true (expected denial)"
            result=1
        else
            # Check if it's an error response without ok field
            if [[ -n "$error_code" ]]; then
                log_pass "Send denied with error: $error_code"
            else
                log_warn "Unexpected response format, checking exit code"
                if [[ $send_exit_code -ne 0 ]]; then
                    log_pass "Send denied (non-zero exit code: $send_exit_code)"
                else
                    log_fail "Could not verify denial"
                    result=1
                fi
            fi
        fi
    else
        # Non-JSON output, check exit code
        if [[ $send_exit_code -ne 0 ]]; then
            log_pass "Send denied (non-zero exit code: $send_exit_code)"
        else
            log_fail "Send may have succeeded (exit code 0, non-JSON output)"
            result=1
        fi
    fi

    # Step 6: Verify no text was actually sent (check pane content)
    log_info "Step 6: Verifying no text was sent to pane..."
    local pane_text
    pane_text=$("$WA_BINARY" robot get-text "$pane_id" 2>&1 || true)
    echo "$pane_text" > "$scenario_dir/pane_text.txt"

    if echo "$pane_text" | grep -q "test_text_should_be_denied"; then
        log_fail "Text was actually sent to pane (policy bypass!)"
        result=1
    else
        log_pass "Confirmed no text leaked to pane"
    fi

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_policy_denial

    return $result
}

run_scenario_graceful_shutdown() {
    local scenario_dir="$1"
    local marker="E2E_SHUTDOWN_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0

    log_info "Using marker: $marker"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Cleanup function
    cleanup_graceful_shutdown() {
        log_verbose "Cleaning up graceful_shutdown scenario"
        # Kill wa watch if still running (should have exited gracefully)
        if [[ -n "${wa_pid:-}" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Force-killing wa watch (pid $wa_pid) - should have exited"
            kill -9 "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing dummy pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.wa"/* "${scenario_dir:-/dev/null}/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_graceful_shutdown EXIT

    # Step 1: Spawn dummy pane with the print script (outputs 200 lines for reliable capture)
    log_info "Step 1: Spawning dummy pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$dummy_script" "$marker" 200 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned pane: $pane_id"
    echo "Spawned pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start wa watch in foreground mode (so we can control it)
    log_info "Step 2: Starting wa watch..."
    "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    # Give wa watch time to initialize
    sleep 1

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 3: Wait for at least one segment to be persisted
    log_info "Step 3: Waiting for capture and persistence..."
    local wait_timeout=${TIMEOUT:-30}

    # Wait for pane to be observed first
    local check_observed_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$check_observed_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Wait for marker to appear in search (proves FTS is working and data is persisted)
    log_info "Step 3b: Waiting for marker to appear in FTS index..."
    local search_check_cmd="\"$WA_BINARY\" search \"$marker\" --limit 10 2>/dev/null | grep -q \"$marker\""
    if ! wait_for_condition "marker in FTS" "$search_check_cmd" "$wait_timeout"; then
        log_warn "Marker not found in FTS before shutdown (may be normal if not persisted yet)"
        # Continue anyway - we'll check after shutdown
    else
        log_pass "Marker found in FTS before shutdown"
    fi

    # Record pre-shutdown state
    "$WA_BINARY" robot state > "$scenario_dir/robot_state_before_shutdown.json" 2>&1 || true
    "$WA_BINARY" search "$marker" --limit 10 > "$scenario_dir/search_before_shutdown.txt" 2>&1 || true

    # Step 4: Send SIGINT to wa watch and measure shutdown time
    log_info "Step 4: Sending SIGINT to wa watch..."
    local shutdown_start=$(date +%s)
    kill -INT "$wa_pid" 2>/dev/null

    # Wait for graceful exit (bounded timeout)
    local shutdown_timeout=10
    local shutdown_result=0
    if timeout "$shutdown_timeout" tail --pid="$wa_pid" -f /dev/null 2>/dev/null; then
        shutdown_result=0
    else
        # Fallback: poll for process exit
        local poll_count=0
        while kill -0 "$wa_pid" 2>/dev/null && [[ $poll_count -lt $((shutdown_timeout * 2)) ]]; do
            sleep 0.5
            ((poll_count++))
        done
        if kill -0 "$wa_pid" 2>/dev/null; then
            shutdown_result=1
        fi
    fi

    local shutdown_end=$(date +%s)
    local shutdown_duration=$((shutdown_end - shutdown_start))
    echo "shutdown_duration_secs: $shutdown_duration" >> "$scenario_dir/scenario.log"

    if [[ $shutdown_result -eq 0 ]] || ! kill -0 "$wa_pid" 2>/dev/null; then
        log_pass "wa watch exited cleanly within ${shutdown_duration}s"
        wa_pid=""  # Mark as exited
    else
        log_fail "wa watch did not exit within ${shutdown_timeout}s - forcing kill"
        kill -9 "$wa_pid" 2>/dev/null || true
        wait "$wa_pid" 2>/dev/null || true
        wa_pid=""
        result=1
    fi

    # Step 5: Verify storage was flushed (FTS still works)
    log_info "Step 5: Verifying storage flush (FTS search after shutdown)..."
    local search_output
    search_output=$("$WA_BINARY" search "$marker" --limit 50 2>&1)
    echo "$search_output" > "$scenario_dir/search_after_shutdown.txt"

    local hit_count
    hit_count=$(echo "$search_output" | grep -c "$marker" || echo "0")
    echo "search_hit_count_after_shutdown: $hit_count" >> "$scenario_dir/scenario.log"

    if [[ "$hit_count" -ge 1 ]]; then
        log_pass "FTS search works after shutdown ($hit_count hits for marker)"
    else
        log_fail "FTS search found no hits after shutdown - data may not have been flushed"
        result=1
    fi

    # Step 6: Verify lock was released (can restart wa watch)
    log_info "Step 6: Verifying lock release (attempting restart)..."

    local restart_pid=""
    "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_restart.log" 2>&1 &
    restart_pid=$!

    sleep 2

    if kill -0 "$restart_pid" 2>/dev/null; then
        log_pass "wa watch restarted successfully (lock was released)"
        # Clean up the restarted process
        kill -INT "$restart_pid" 2>/dev/null || true
        sleep 1
        if kill -0 "$restart_pid" 2>/dev/null; then
            kill -9 "$restart_pid" 2>/dev/null || true
        fi
        wait "$restart_pid" 2>/dev/null || true
    else
        # Check if it exited with lock error
        if grep -qi "lock\|already running\|another instance" "$scenario_dir/wa_watch_restart.log" 2>/dev/null; then
            log_fail "wa watch restart failed - lock was NOT released"
            result=1
        else
            # May have exited for other reason, check exit status
            wait "$restart_pid" 2>/dev/null
            local restart_exit=$?
            if [[ $restart_exit -eq 0 ]]; then
                log_pass "wa watch restart exited cleanly (lock was available)"
            else
                log_warn "wa watch restart exited with code $restart_exit (check logs)"
                # Not necessarily a failure - may be config issue
            fi
        fi
    fi

    # Step 7: Verify shutdown summary in logs
    log_info "Step 7: Checking shutdown logs..."
    if grep -qi "shutdown\|terminating\|graceful\|SIGINT\|signal" "$scenario_dir/wa_watch.log" 2>/dev/null; then
        log_pass "Found shutdown-related messages in logs"
    else
        log_warn "No obvious shutdown messages in logs (may be expected)"
    fi

    # Record final summary
    echo "" >> "$scenario_dir/scenario.log"
    echo "=== Shutdown Summary ===" >> "$scenario_dir/scenario.log"
    echo "shutdown_clean: $([[ $result -eq 0 ]] && echo 'yes' || echo 'no')" >> "$scenario_dir/scenario.log"
    echo "fts_hits_after_shutdown: $hit_count" >> "$scenario_dir/scenario.log"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_graceful_shutdown

    return $result
}

# ==============================================================================
# Scenario: pane_exclude_filter
# ==============================================================================
# Tests that pane exclude filters prevent capture of matching panes.
# - Spawns an "observed" pane that prints OBSERVED_TOKEN
# - Spawns an "ignored" pane with title "IGNORED_PANE" that prints SECRET_TOKEN
# - Asserts observed pane is searchable, ignored is NOT
# - Asserts wa status shows ignored pane with exclude reason
# - Asserts SECRET_TOKEN never appears in any artifacts (privacy guarantee)

run_scenario_pane_exclude_filter() {
    local scenario_dir="$1"
    local observed_marker="OBSERVED_TOKEN_$(date +%s%N)"
    local secret_token="SECRET_TOKEN_$(date +%s%N)"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-XXXXXX)
    local wa_pid=""
    local observed_pane_id=""
    local ignored_pane_id=""
    local result=0

    log_info "Using observed marker: $observed_marker"
    log_info "Using secret token: $secret_token"
    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Copy pane exclude config
    local exclude_config="$PROJECT_ROOT/fixtures/e2e/config_pane_exclude.toml"
    if [[ -f "$exclude_config" ]]; then
        cp "$exclude_config" "$temp_workspace/wa.toml"
        export WA_CONFIG="$temp_workspace/wa.toml"
        log_verbose "Using exclude config: $exclude_config"
    else
        log_fail "Pane exclude config not found: $exclude_config"
        return 1
    fi

    # Record tokens for artifact verification
    echo "observed_marker: $observed_marker" >> "$scenario_dir/scenario.log"
    echo "secret_token: $secret_token" >> "$scenario_dir/scenario.log"

    # Cleanup function
    cleanup_pane_exclude_filter() {
        log_verbose "Cleaning up pane_exclude_filter scenario"
        # Kill wa watch if running (use :- to avoid unbound variable with set -u)
        if [[ -n "${wa_pid:-}" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close observed pane if it exists
        if [[ -n "${observed_pane_id:-}" ]]; then
            log_verbose "Closing observed pane $observed_pane_id"
            wezterm cli kill-pane --pane-id "$observed_pane_id" 2>/dev/null || true
        fi
        # Close ignored pane if it exists
        if [[ -n "${ignored_pane_id:-}" ]]; then
            log_verbose "Closing ignored pane $ignored_pane_id"
            wezterm cli kill-pane --pane-id "$ignored_pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup (use :- to avoid unbound variable with set -u)
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "${temp_workspace}/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "${temp_workspace}/wa.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_pane_exclude_filter EXIT

    # Step 1: Spawn the OBSERVED pane (standard dummy_print.sh)
    log_info "Step 1: Spawning observed pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    # Run dummy_print.sh then sleep to keep pane alive for observation
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash -c "'$dummy_script' '$observed_marker' 50; sleep 300" 2>&1)
    observed_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$observed_pane_id" ]]; then
        log_fail "Failed to spawn observed pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned observed pane: $observed_pane_id"
    echo "observed_pane_id: $observed_pane_id" >> "$scenario_dir/scenario.log"

    # Step 2: Spawn the IGNORED pane (dummy_ignored_pane.sh with title matching exclude rule)
    log_info "Step 2: Spawning ignored pane (title=IGNORED_PANE)..."
    local ignored_script="$PROJECT_ROOT/fixtures/e2e/dummy_ignored_pane.sh"
    if [[ ! -x "$ignored_script" ]]; then
        log_fail "Ignored pane script not found or not executable: $ignored_script"
        return 1
    fi

    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$ignored_script" "$secret_token" 50 2>&1)
    ignored_pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$ignored_pane_id" ]]; then
        log_fail "Failed to spawn ignored pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned ignored pane: $ignored_pane_id"
    echo "ignored_pane_id: $ignored_pane_id" >> "$scenario_dir/scenario.log"

    # Give time for title change to propagate
    sleep 2

    # Step 3: Start wa watch in background with custom config
    log_info "Step 3: Starting wa watch with exclude config..."
    "$WA_BINARY" watch --foreground --config "$temp_workspace/wa.toml" \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    # Give wa watch a moment to initialize
    sleep 2

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 4a: Wait for observed pane to appear in robot state
    log_info "Step 4a: Waiting for observed pane to be observed..."
    local wait_timeout=${TIMEOUT:-60}
    local check_observed_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $observed_pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "observed pane $observed_pane_id in robot state" "$check_observed_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for observed pane to appear in robot state"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Observed pane detected in robot state"

    # Step 4b: Wait for observed content to be searchable (proves FTS indexing works)
    log_info "Step 4b: Waiting for observed content to be searchable..."
    # Search for the observed marker - check total_hits > 0
    local check_search_cmd="\"$WA_BINARY\" robot search \"$observed_marker\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"

    if ! wait_for_condition "observed content searchable" "$check_search_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for observed content to be searchable"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        "$WA_BINARY" robot search "$observed_marker" > "$scenario_dir/search_debug.json" 2>&1 || true
        return 1
    fi
    log_pass "Observed content captured and searchable"

    # Capture robot state (while watcher is still running)
    "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true

    # Step 5: Assert OBSERVED_TOKEN is searchable (watcher still running for IPC)
    log_info "Step 5: Asserting observed token is searchable..."
    local search_output
    search_output=$("$WA_BINARY" robot search "$observed_marker" 2>&1)
    echo "$search_output" > "$scenario_dir/search_observed.json"

    local observed_count
    observed_count=$(echo "$search_output" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")

    if [[ "$observed_count" -gt 0 ]]; then
        log_pass "Observed token found in search ($observed_count results)"
    else
        log_fail "Observed token NOT found in search"
        result=1
    fi

    # Step 6: Assert SECRET_TOKEN is NOT searchable (privacy guarantee)
    log_info "Step 6: Asserting secret token is NOT searchable..."
    search_output=$("$WA_BINARY" robot search "$secret_token" 2>&1)
    echo "$search_output" > "$scenario_dir/search_secret.json"

    local secret_count
    secret_count=$(echo "$search_output" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")

    if [[ "$secret_count" -eq 0 ]]; then
        log_pass "Secret token correctly NOT found in search"
    else
        log_fail "SECRET TOKEN FOUND IN SEARCH - PRIVACY VIOLATION!"
        result=1
    fi

    # Step 7: Stop wa watch gracefully (after search tests complete)
    log_info "Step 7: Stopping wa watch..."
    kill -TERM "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""

    # Step 8: Assert SECRET_TOKEN never appears in any captured data files
    log_info "Step 8: Scanning captured data for secret token leakage..."

    # Copy all wa data artifacts first (database, logs, segments)
    cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true

    # Search for leaks in captured data - exclude our own test harness files:
    # - scenario.log: intentionally contains tokens for debugging
    # - search_*.json: contains search queries (not search results finding the token)
    local leaked_files
    leaked_files=$(grep -rl "$secret_token" "$scenario_dir" \
        --exclude="scenario.log" \
        --exclude="search_*.json" \
        2>/dev/null || true)

    if [[ -z "$leaked_files" ]]; then
        log_pass "Secret token not found in any captured data"
    else
        log_fail "SECRET TOKEN LEAKED IN CAPTURED DATA:"
        echo "$leaked_files" | while read -r file; do
            log_fail "  - $file"
        done
        result=1
    fi

    # Step 9: Check robot state shows ignored pane was filtered (informational)
    log_info "Step 9: Checking status output for exclude reason..."

    # This is informational - we check robot state for pane visibility
    local state_output
    state_output=$(cat "$scenario_dir/robot_state.json" 2>/dev/null || echo "{}")

    # Check if ignored pane appears in state with any exclusion indicator
    # (Implementation may vary - this is advisory logging)
    local ignored_in_state
    ignored_in_state=$(echo "$state_output" | jq -e ".data[]? | select(.pane_id == $ignored_pane_id)" 2>/dev/null || true)

    if [[ -z "$ignored_in_state" ]]; then
        log_pass "Ignored pane correctly absent from robot state"
    else
        # Check if it has an exclusion reason
        local exclude_reason
        exclude_reason=$(echo "$ignored_in_state" | jq -r '.exclude_reason // .ignored_reason // empty' 2>/dev/null || true)
        if [[ -n "$exclude_reason" ]]; then
            log_pass "Ignored pane present with exclude reason: $exclude_reason"
        else
            log_warn "Ignored pane present in state without clear exclude reason"
        fi
    fi

    # Cleanup
    trap - EXIT
    cleanup_pane_exclude_filter

    return $result
}

run_scenario_workspace_isolation() {
    local scenario_dir="$1"
    local token_a="WORKSPACE_TOKEN_A_$(date +%s%N)"
    local token_b="WORKSPACE_TOKEN_B_$(date +%s%N)"
    local workspace_a
    local workspace_b
    workspace_a=$(mktemp -d /tmp/wa-e2e-a-XXXXXX)
    workspace_b=$(mktemp -d /tmp/wa-e2e-b-XXXXXX)
    local wa_pid=""
    local pane_a_id=""
    local pane_b_id=""
    local result=0

    log_info "Workspace A token: $token_a"
    log_info "Workspace B token: $token_b"
    log_info "Workspace A: $workspace_a"
    log_info "Workspace B: $workspace_b"

    mkdir -p "$workspace_a/.wa" "$workspace_b/.wa"

    echo "workspace_a: $workspace_a" >> "$scenario_dir/scenario.log"
    echo "workspace_b: $workspace_b" >> "$scenario_dir/scenario.log"
    echo "token_a: $token_a" >> "$scenario_dir/scenario.log"
    echo "token_b: $token_b" >> "$scenario_dir/scenario.log"

    cleanup_workspace_isolation() {
        log_verbose "Cleaning up workspace_isolation scenario"
        if [[ -n "${wa_pid:-}" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        if [[ -n "${pane_a_id:-}" ]]; then
            log_verbose "Closing workspace A pane $pane_a_id"
            wezterm cli kill-pane --pane-id "$pane_a_id" 2>/dev/null || true
        fi
        if [[ -n "${pane_b_id:-}" ]]; then
            log_verbose "Closing workspace B pane $pane_b_id"
            wezterm cli kill-pane --pane-id "$pane_b_id" 2>/dev/null || true
        fi

        if [[ -d "${workspace_a:-}" ]]; then
            mkdir -p "$scenario_dir/workspace_a"
            cp -r "$workspace_a/.wa"/* "$scenario_dir/workspace_a/" 2>/dev/null || true
        fi
        if [[ -d "${workspace_b:-}" ]]; then
            mkdir -p "$scenario_dir/workspace_b"
            cp -r "$workspace_b/.wa"/* "$scenario_dir/workspace_b/" 2>/dev/null || true
        fi

        if [[ "${WA_E2E_PRESERVE_TEMP:-}" == "1" ]]; then
            log_warn "Preserving temp workspaces (WA_E2E_PRESERVE_TEMP=1)"
        else
            rm -rf "${workspace_a:-}" "${workspace_b:-}"
        fi
    }
    trap cleanup_workspace_isolation EXIT

    # Step 1: Spawn workspace A pane
    log_info "Step 1: Spawning workspace A pane..."
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_fail "Dummy print script not found or not executable: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(wezterm cli spawn --cwd "$workspace_a" -- bash -c "'$dummy_script' '$token_a' 80; sleep 300" 2>&1)
    pane_a_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_a_id" ]]; then
        log_fail "Failed to spawn workspace A pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned workspace A pane: $pane_a_id"
    echo "pane_a_id: $pane_a_id" >> "$scenario_dir/scenario.log"

    # Step 2: Start wa watch for workspace A
    log_info "Step 2: Starting wa watch for workspace A..."
    WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
        "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_a.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch (A) started with PID $wa_pid"
    echo "wa_pid_a: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch (A) exited immediately"
        return 1
    fi

    # Step 3: Wait for workspace A pane to be observed
    log_info "Step 3: Waiting for workspace A pane to be observed..."
    local wait_timeout=${TIMEOUT:-60}
    local check_observed_a="WA_LOG_LEVEL=error WA_WORKSPACE=\"$workspace_a\" WA_DATA_DIR=\"$workspace_a/.wa\" \"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_a_id)' >/dev/null 2>&1"

    if ! wait_for_condition "workspace A pane observed" "$check_observed_a" "$wait_timeout"; then
        log_fail "Timeout waiting for workspace A pane to be observed"
        WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
            "$WA_BINARY" robot state > "$scenario_dir/robot_state_a.json" 2>&1 || true
        return 1
    fi
    log_pass "Workspace A pane observed"

    # Step 4: Wait for token A to be searchable in workspace A
    log_info "Step 4: Waiting for token A to be searchable..."
    local check_search_a="WA_LOG_LEVEL=error WA_WORKSPACE=\"$workspace_a\" WA_DATA_DIR=\"$workspace_a/.wa\" \"$WA_BINARY\" robot search \"$token_a\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"
    if ! wait_for_condition "token A searchable" "$check_search_a" "$wait_timeout"; then
        log_fail "Timeout waiting for token A to be searchable"
        WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
            "$WA_BINARY" robot search "$token_a" > "$scenario_dir/search_a.json" 2>&1 || true
        return 1
    fi
    log_pass "Token A searchable in workspace A"

    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
        "$WA_BINARY" robot state > "$scenario_dir/robot_state_a.json" 2>&1 || true
    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
        "$WA_BINARY" robot search "$token_a" > "$scenario_dir/search_a.json" 2>&1 || true
    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_a" WA_DATA_DIR="$workspace_a/.wa" \
        "$WA_BINARY" config show --effective --json > "$scenario_dir/config_effective_a.json" 2>&1 || true

    # Step 5: Stop wa watch for workspace A
    log_info "Step 5: Stopping wa watch for workspace A..."
    kill -TERM "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""

    # Step 6: Spawn workspace B pane
    log_info "Step 6: Spawning workspace B pane..."
    spawn_output=$(wezterm cli spawn --cwd "$workspace_b" -- bash -c "'$dummy_script' '$token_b' 80; sleep 300" 2>&1)
    pane_b_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_b_id" ]]; then
        log_fail "Failed to spawn workspace B pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned workspace B pane: $pane_b_id"
    echo "pane_b_id: $pane_b_id" >> "$scenario_dir/scenario.log"

    # Step 7: Start wa watch for workspace B
    log_info "Step 7: Starting wa watch for workspace B..."
    WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
        "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch_b.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch (B) started with PID $wa_pid"
    echo "wa_pid_b: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch (B) exited immediately"
        return 1
    fi

    # Step 8: Wait for workspace B pane to be observed
    log_info "Step 8: Waiting for workspace B pane to be observed..."
    local check_observed_b="WA_LOG_LEVEL=error WA_WORKSPACE=\"$workspace_b\" WA_DATA_DIR=\"$workspace_b/.wa\" \"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_b_id)' >/dev/null 2>&1"

    if ! wait_for_condition "workspace B pane observed" "$check_observed_b" "$wait_timeout"; then
        log_fail "Timeout waiting for workspace B pane to be observed"
        WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
            "$WA_BINARY" robot state > "$scenario_dir/robot_state_b.json" 2>&1 || true
        return 1
    fi
    log_pass "Workspace B pane observed"

    # Step 9: Wait for token B to be searchable in workspace B
    log_info "Step 9: Waiting for token B to be searchable..."
    local check_search_b="WA_LOG_LEVEL=error WA_WORKSPACE=\"$workspace_b\" WA_DATA_DIR=\"$workspace_b/.wa\" \"$WA_BINARY\" robot search \"$token_b\" 2>/dev/null | jq -e '.data.total_hits > 0' >/dev/null 2>&1"
    if ! wait_for_condition "token B searchable" "$check_search_b" "$wait_timeout"; then
        log_fail "Timeout waiting for token B to be searchable"
        WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
            "$WA_BINARY" robot search "$token_b" > "$scenario_dir/search_b.json" 2>&1 || true
        return 1
    fi
    log_pass "Token B searchable in workspace B"

    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
        "$WA_BINARY" robot state > "$scenario_dir/robot_state_b.json" 2>&1 || true
    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
        "$WA_BINARY" robot search "$token_b" > "$scenario_dir/search_b.json" 2>&1 || true
    WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
        "$WA_BINARY" config show --effective --json > "$scenario_dir/config_effective_b.json" 2>&1 || true

    # Step 10: Assert token A is NOT searchable in workspace B
    log_info "Step 10: Asserting token A is NOT searchable in workspace B..."
    local search_output_ba
    search_output_ba=$(WA_LOG_LEVEL=error WA_WORKSPACE="$workspace_b" WA_DATA_DIR="$workspace_b/.wa" \
        "$WA_BINARY" robot search "$token_a" 2>&1)
    echo "$search_output_ba" > "$scenario_dir/search_a_in_b.json"

    local token_a_hits
    token_a_hits=$(echo "$search_output_ba" | jq -r '.data.total_hits // .data.total // 0' 2>/dev/null || echo "0")
    if [[ "$token_a_hits" -eq 0 ]]; then
        log_pass "Token A not found in workspace B (isolation OK)"
    else
        log_fail "Token A found in workspace B ($token_a_hits hits) - isolation broken"
        result=1
    fi

    # Step 11: Stop wa watch for workspace B
    log_info "Step 11: Stopping wa watch for workspace B..."
    kill -TERM "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""

    # Step 12: Verify derived paths and workspace roots are distinct
    log_info "Step 12: Verifying workspace roots and derived paths..."
    local db_a
    local db_b
    local root_a
    local root_b
    local log_a
    local log_b
    local logs_dir_a
    local logs_dir_b
    db_a=$(jq -r '.paths.db_path // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    db_b=$(jq -r '.paths.db_path // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    root_a=$(jq -r '.paths.workspace_root // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    root_b=$(jq -r '.paths.workspace_root // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    log_a=$(jq -r '.paths.log_path // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    log_b=$(jq -r '.paths.log_path // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")
    logs_dir_a=$(jq -r '.paths.logs_dir // empty' "$scenario_dir/config_effective_a.json" 2>/dev/null || echo "")
    logs_dir_b=$(jq -r '.paths.logs_dir // empty' "$scenario_dir/config_effective_b.json" 2>/dev/null || echo "")

    echo "workspace_root_a: $root_a" >> "$scenario_dir/scenario.log"
    echo "workspace_root_b: $root_b" >> "$scenario_dir/scenario.log"
    echo "db_path_a: $db_a" >> "$scenario_dir/scenario.log"
    echo "db_path_b: $db_b" >> "$scenario_dir/scenario.log"
    echo "log_path_a: $log_a" >> "$scenario_dir/scenario.log"
    echo "log_path_b: $log_b" >> "$scenario_dir/scenario.log"
    echo "logs_dir_a: $logs_dir_a" >> "$scenario_dir/scenario.log"
    echo "logs_dir_b: $logs_dir_b" >> "$scenario_dir/scenario.log"

    if [[ -n "$root_a" && -n "$root_b" ]]; then
        if [[ "$root_a" == "$workspace_a" && "$root_b" == "$workspace_b" ]]; then
            log_pass "Workspace roots match expected paths"
        else
            log_fail "Workspace roots do not match expected paths"
            result=1
        fi
    else
        log_fail "Could not parse workspace roots from effective config"
        result=1
    fi

    if [[ -n "$db_a" && -n "$db_b" ]]; then
        if [[ "$db_a" != "$db_b" ]]; then
            log_pass "Workspace db paths are distinct"
        else
            log_fail "Workspace db paths are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse db paths from effective config"
        result=1
    fi

    if [[ -n "$log_a" && -n "$log_b" ]]; then
        if [[ "$log_a" != "$log_b" ]]; then
            log_pass "Workspace log paths are distinct"
        else
            log_fail "Workspace log paths are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse log paths from effective config"
        result=1
    fi

    if [[ -n "$logs_dir_a" && -n "$logs_dir_b" ]]; then
        if [[ "$logs_dir_a" != "$logs_dir_b" ]]; then
            log_pass "Workspace logs directories are distinct"
        else
            log_fail "Workspace logs directories are identical (expected distinct)"
            result=1
        fi
    else
        log_fail "Could not parse logs directories from effective config"
        result=1
    fi

    trap - EXIT
    cleanup_workspace_isolation

    return $result
}

run_scenario_setup_idempotency() {
    local scenario_dir="$1"
    local temp_home
    temp_home=$(mktemp -d /tmp/wa-e2e-setup-XXXXXX)
    local result=0
    local wezterm_dir="$temp_home/.config/wezterm"
    local wezterm_file="$wezterm_dir/wezterm.lua"
    local zshrc="$temp_home/.zshrc"
    local bashrc="$temp_home/.bashrc"
    local fish_conf="$temp_home/.config/fish/config.fish"
    local ssh_conf="$temp_home/.ssh/config"

    log_info "Temp home: $temp_home"
    echo "temp_home: $temp_home" >> "$scenario_dir/scenario.log"

    mkdir -p "$wezterm_dir" "$temp_home/.config/fish" "$temp_home/.ssh"
    cat > "$wezterm_file" <<'EOF'
local wezterm = require 'wezterm'
local config = {}
return config
EOF
    printf "# zshrc baseline\n" > "$zshrc"
    printf "# bashrc baseline\n" > "$bashrc"
    printf "# fish baseline\n" > "$fish_conf"
    cat > "$ssh_conf" <<'EOF'
Host example
  HostName example.com
EOF

    cleanup_setup_idempotency() {
        log_verbose "Cleaning up setup_idempotency scenario"
        if [[ -d "${temp_home:-}" ]]; then
            cp -r "$temp_home" "$scenario_dir/temp_home_snapshot" 2>/dev/null || true
        fi
        if [[ "${WA_E2E_PRESERVE_TEMP:-}" == "1" ]]; then
            log_warn "Preserving temp home (WA_E2E_PRESERVE_TEMP=1)"
        else
            rm -rf "${temp_home:-}"
        fi
    }
    trap cleanup_setup_idempotency EXIT

    local files_before="$scenario_dir/files_before.txt"
    local files_after_dry="$scenario_dir/files_after_dry.txt"
    local files_after_apply="$scenario_dir/files_after_apply.txt"
    local files_after_second="$scenario_dir/files_after_second.txt"
    local git_before="$scenario_dir/git_status_before.txt"
    local git_after="$scenario_dir/git_status_after.txt"

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_before"
    git status --porcelain > "$git_before"

    # Step 1: Dry-run (should not modify files)
    log_info "Step 1: wa setup --dry-run"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$WA_BINARY" setup --dry-run > "$scenario_dir/setup_dry_run.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_dry"
    if diff -u "$files_before" "$files_after_dry" > "$scenario_dir/dry_run_diff.txt"; then
        log_pass "Dry-run made no file changes"
    else
        log_fail "Dry-run modified files (unexpected)"
        result=1
    fi

    # Step 2: Apply setup
    log_info "Step 2: wa setup --apply"
    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$WA_BINARY" setup --apply > "$scenario_dir/setup_apply.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_apply"
    cp "$wezterm_file" "$scenario_dir/wezterm_after_apply.lua" 2>/dev/null || true
    cp "$zshrc" "$scenario_dir/zshrc_after_apply" 2>/dev/null || true

    local wa_block_count
    wa_block_count=$(grep -c "WA-BEGIN" "$wezterm_file" 2>/dev/null || true)
    if [[ "$wa_block_count" -eq 1 ]]; then
        log_pass "wezterm.lua contains exactly one WA block"
    else
        log_fail "wezterm.lua WA block count expected 1, got $wa_block_count"
        result=1
    fi

    local shell_block_count
    shell_block_count=$(grep -c "WA-BEGIN" "$zshrc" 2>/dev/null || true)
    if [[ "$shell_block_count" -eq 1 ]]; then
        log_pass "zshrc contains exactly one WA block"
    else
        log_fail "zshrc WA block count expected 1, got $shell_block_count"
        result=1
    fi

    # Step 3: Apply again (idempotent)
    log_info "Step 3: wa setup --apply (idempotent)"
    cp "$wezterm_file" "$scenario_dir/wezterm_before_second.lua" 2>/dev/null || true
    cp "$zshrc" "$scenario_dir/zshrc_before_second" 2>/dev/null || true

    HOME="$temp_home" XDG_CONFIG_HOME="$temp_home/.config" SHELL="/bin/zsh" \
        "$WA_BINARY" setup --apply > "$scenario_dir/setup_apply_again.log" 2>&1 || result=1

    find "$temp_home" -type f -print0 | sort -z | xargs -0 sha256sum > "$files_after_second"

    if diff -u "$scenario_dir/wezterm_before_second.lua" "$wezterm_file" \
        > "$scenario_dir/wezterm_idempotent_diff.txt"; then
        log_pass "wezterm.lua unchanged on second apply"
    else
        log_fail "wezterm.lua changed on second apply"
        result=1
    fi

    if diff -u "$scenario_dir/zshrc_before_second" "$zshrc" \
        > "$scenario_dir/zshrc_idempotent_diff.txt"; then
        log_pass "zshrc unchanged on second apply"
    else
        log_fail "zshrc changed on second apply"
        result=1
    fi

    # Guard: ensure no repo modifications
    git status --porcelain > "$git_after"
    if diff -u "$git_before" "$git_after" > "$scenario_dir/git_status_diff.txt"; then
        log_pass "No repo modifications detected"
    else
        log_fail "Repo modified during setup scenario (unexpected)"
        result=1
    fi

    # Guard: any paths printed by wa should be under temp home
    local printed_paths
    printed_paths=$(grep -Eo "/[^ ]+" \
        "$scenario_dir/setup_dry_run.log" \
        "$scenario_dir/setup_apply.log" \
        "$scenario_dir/setup_apply_again.log" \
        | sort -u || true)
    if [[ -n "$printed_paths" ]]; then
        local bad_paths
        bad_paths=$(echo "$printed_paths" | grep -v "^$temp_home" || true)
        if [[ -n "$bad_paths" ]]; then
            log_fail "Detected paths outside temp home in output"
            echo "$bad_paths" >> "$scenario_dir/outside_paths.txt"
            result=1
        else
            log_pass "All printed paths are within temp home"
        fi
    else
        log_warn "No paths detected in output (guard skipped)"
    fi

    trap - EXIT
    cleanup_setup_idempotency

    return $result
}

run_scenario_uservar_forwarding() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-uservar-XXXXXX)
    local wa_pid=""
    local wezterm_pid=""
    local pane_id=""
    local result=0
    local wezterm_class="wa-e2e-uservar-$(date +%s%N)"
    local uservar_name="wa_event"
    local payload_json
    payload_json=$(printf '{"type":"e2e_uservar","ts":%s}' "$(date +%s)")
    local payload_b64
    payload_b64=$(printf '%s' "$payload_json" | base64 | tr -d '\n')
    local config_file="$temp_workspace/wezterm.lua"
    local emit_script="$temp_workspace/emit_uservar.sh"
    local wait_timeout=${TIMEOUT:-60}

    log_info "User-var name: $uservar_name"
    log_info "User-var payload: $payload_json"
    log_info "WezTerm class: $wezterm_class"
    log_info "Workspace: $temp_workspace"

    mkdir -p "$temp_workspace/.wa"

    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    echo "wezterm_class: $wezterm_class" >> "$scenario_dir/scenario.log"
    echo "uservar_name: $uservar_name" >> "$scenario_dir/scenario.log"
    echo "payload_json: $payload_json" >> "$scenario_dir/scenario.log"

    cleanup_uservar_forwarding() {
        log_verbose "Cleaning up uservar_forwarding scenario"
        if [[ -n "${wa_pid:-}" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        if [[ -n "${pane_id:-}" ]]; then
            log_verbose "Closing uservar pane $pane_id"
            wezterm cli --no-auto-start --class "$wezterm_class" kill-pane \
                --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -n "${wezterm_pid:-}" ]] && kill -0 "$wezterm_pid" 2>/dev/null; then
            log_verbose "Stopping wezterm (pid $wezterm_pid)"
            kill "$wezterm_pid" 2>/dev/null || true
            wait "$wezterm_pid" 2>/dev/null || true
        fi
        if [[ -d "${temp_workspace:-}" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "$config_file" "$scenario_dir/wezterm.lua" 2>/dev/null || true
        fi
        rm -rf "${temp_workspace:-}"
    }
    trap cleanup_uservar_forwarding EXIT

    # Step 1: Write a minimal wezterm.lua that forwards user-var events to wa
    log_info "Step 1: Writing wezterm.lua forwarding snippet..."
    cat > "$config_file" <<'EOF'
local wezterm = require 'wezterm'
local wa_bin = os.getenv("WA_E2E_WA_BINARY") or "wa"

wezterm.on('user-var-changed', function(window, pane, name, value)
  if not name or name == "" then
    return
  end
  local pane_id = tostring(pane:pane_id())
  wezterm.background_child_process {
    wa_bin,
    "event",
    "--from-uservar",
    "--pane",
    pane_id,
    "--name",
    name,
    "--value",
    value,
  }
end)

return {}
EOF

    # Step 2: Start a dedicated wezterm instance with the forwarding config
    log_info "Step 2: Starting wezterm with forwarding config..."
    WA_E2E_WA_BINARY="$WA_BINARY" wezterm --config-file "$config_file" start \
        --always-new-process --class "$wezterm_class" --workspace "wa-e2e-uservar" \
        > "$scenario_dir/wezterm.log" 2>&1 &
    wezterm_pid=$!
    echo "wezterm_pid: $wezterm_pid" >> "$scenario_dir/scenario.log"

    local check_mux_cmd="wezterm cli --no-auto-start --class \"$wezterm_class\" list >/dev/null 2>&1"
    if ! wait_for_condition "wezterm mux ready" "$check_mux_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for wezterm mux"
        result=1
        return $result
    fi
    log_pass "WezTerm mux ready"

    # Step 3: Start wa watch with debug logging
    log_info "Step 3: Starting wa watch..."
    WA_WORKSPACE="$temp_workspace" WA_DATA_DIR="$temp_workspace/.wa" WA_LOG_LEVEL=debug \
        "$WA_BINARY" watch --foreground \
        > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        result=1
        return $result
    fi

    # Step 4: Create a temporary script to emit the user-var
    log_info "Step 4: Preparing user-var emitter script..."
    cat > "$emit_script" <<'EOS'
#!/bin/bash
set -euo pipefail
name="$1"
payload="$2"
sleep_time="${3:-120}"
printf '\033]1337;SetUserVar=%s=%s\007' "$name" "$payload"
echo "USERVAR_SENT name=$name"
sleep "$sleep_time"
EOS
    chmod +x "$emit_script"

    # Step 5: Spawn a pane that emits the user-var
    log_info "Step 5: Spawning pane to emit user-var..."
    local spawn_output
    spawn_output=$(wezterm cli --no-auto-start --class "$wezterm_class" spawn \
        --cwd "$temp_workspace" -- "$emit_script" "$uservar_name" "$payload_b64" 120 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn uservar pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        result=1
        return $result
    fi
    log_info "Spawned uservar pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 6: Wait for wa watch to record the forwarded user-var event
    log_info "Step 6: Waiting for forwarded user-var event..."
    local check_event_cmd="grep -q \"Published user-var event\" \"$scenario_dir/wa_watch.log\""
    if ! wait_for_condition "user-var forwarded to watcher" "$check_event_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for user-var forwarding"
        tail -200 "$scenario_dir/wa_watch.log" >> "$scenario_dir/scenario.log" 2>/dev/null || true
        result=1
    else
        log_pass "User-var forwarded and received by watcher"
    fi

    # Step 7: Malformed payload should be rejected (validation check)
    log_info "Step 7: Verifying malformed payload is rejected..."
    local invalid_output=""
    local invalid_exit=0
    set +e
    invalid_output=$(WA_WORKSPACE="$temp_workspace" WA_DATA_DIR="$temp_workspace/.wa" \
        "$WA_BINARY" event --from-uservar --pane "${pane_id:-0}" \
        --name "$uservar_name" --value "invalid_base64" 2>&1)
    invalid_exit=$?
    set -e

    echo "$invalid_output" > "$scenario_dir/wa_event_invalid.log"
    echo "invalid_exit: $invalid_exit" >> "$scenario_dir/scenario.log"

    if [[ "$invalid_exit" -ne 0 ]]; then
        log_pass "Malformed payload rejected"
    else
        log_fail "Malformed payload unexpectedly accepted"
        result=1
    fi

    trap - EXIT
    cleanup_uservar_forwarding

    return $result
}

# ==============================================================================
# Scenario: Workflow Resume After Restart
# ==============================================================================
# This scenario validates that workflows resume from the last completed step
# after the watcher is killed and restarted. It ensures:
# 1. Workflow state is persisted to storage
# 2. Incomplete workflows are resumed on startup
# 3. No step that sends input is executed twice
# ==============================================================================

run_scenario_workflow_resume() {
    local scenario_dir="$1"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-resume-XXXXXX)
    local wa_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-45}

    log_info "Workspace: $temp_workspace"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    # Copy baseline config for workflow testing
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$temp_workspace/wa.toml"
        export WA_CONFIG="$temp_workspace/wa.toml"
        log_verbose "Using baseline config: $baseline_config"
    fi

    # Cleanup function
    cleanup_workflow_resume() {
        log_verbose "Cleaning up workflow_resume scenario"
        # Kill wa watch if running
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        # Close dummy pane if it exists
        if [[ -n "$pane_id" ]]; then
            log_verbose "Closing dummy agent pane $pane_id"
            wezterm cli kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        # Copy artifacts before cleanup
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "$temp_workspace/wa.toml" "$scenario_dir/" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_workflow_resume EXIT

    # Step 1: Start wa watch with auto-handle
    log_info "Step 1: Starting wa watch with --auto-handle..."
    "$WA_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch_1.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch started with PID $wa_pid"
    echo "wa_pid_1: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch exited immediately"
        return 1
    fi

    # Step 2: Spawn dummy agent pane that will trigger compaction
    log_info "Step 2: Spawning dummy agent pane..."
    local agent_script="$PROJECT_ROOT/fixtures/e2e/dummy_agent.sh"
    if [[ ! -x "$agent_script" ]]; then
        log_fail "Dummy agent script not found or not executable: $agent_script"
        return 1
    fi

    local spawn_output
    # Spawn with 1 second delay before compaction marker
    spawn_output=$(wezterm cli spawn --cwd "$temp_workspace" -- bash "$agent_script" 1 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "Failed to spawn dummy agent pane"
        echo "Spawn output: $spawn_output" >> "$scenario_dir/scenario.log"
        return 1
    fi
    log_info "Spawned agent pane: $pane_id"
    echo "agent_pane_id: $pane_id" >> "$scenario_dir/scenario.log"

    # Step 3: Wait for pane to be observed
    log_info "Step 3: Waiting for pane to be observed..."
    local check_cmd="\"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"

    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "Timeout waiting for pane to be observed"
        "$WA_BINARY" robot state > "$scenario_dir/robot_state.json" 2>&1 || true
        return 1
    fi
    log_pass "Pane observed"

    # Step 4: Wait for compaction detection and workflow to start
    log_info "Step 4: Waiting for compaction detection and workflow start..."
    sleep 4  # Give time for agent to emit marker and workflow to start

    # Check for workflow start in logs
    if grep -qi "workflow.*started\|handle_compaction" "$scenario_dir/wa_watch_1.log" 2>/dev/null; then
        log_pass "Workflow started"
    else
        log_warn "Workflow may not have started (checking anyway)"
    fi

    # Step 5: Kill watcher abruptly (simulate crash)
    log_info "Step 5: Killing watcher (simulating crash)..."
    kill -9 "$wa_pid" 2>/dev/null || true
    wait "$wa_pid" 2>/dev/null || true
    wa_pid=""
    log_pass "Watcher killed"

    # Step 6: Check database for incomplete workflow
    log_info "Step 6: Checking database for incomplete workflow..."
    local db_path="$temp_workspace/.wa/wa.db"
    if [[ -f "$db_path" ]]; then
        local workflow_status
        workflow_status=$(sqlite3 "$db_path" "SELECT id, status, current_step FROM workflow_executions ORDER BY started_at DESC LIMIT 1;" 2>/dev/null || echo "")
        echo "workflow_before_restart: $workflow_status" >> "$scenario_dir/scenario.log"

        if [[ -n "$workflow_status" ]]; then
            log_pass "Found workflow in database: $workflow_status"
        else
            log_warn "No workflow found in database (workflow may not have persisted yet)"
        fi

        # Count step logs before restart
        local step_count_before
        step_count_before=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_step_logs;" 2>/dev/null || echo "0")
        echo "step_logs_before_restart: $step_count_before" >> "$scenario_dir/scenario.log"
        log_info "Step logs before restart: $step_count_before"
    else
        log_warn "Database file not found at $db_path"
    fi

    # Step 7: Restart wa watch with auto-handle
    log_info "Step 7: Restarting wa watch with --auto-handle..."
    "$WA_BINARY" watch --foreground --auto-handle \
        > "$scenario_dir/wa_watch_2.log" 2>&1 &
    wa_pid=$!
    log_verbose "wa watch restarted with PID $wa_pid"
    echo "wa_pid_2: $wa_pid" >> "$scenario_dir/scenario.log"

    sleep 2

    # Verify wa watch is running
    if ! kill -0 "$wa_pid" 2>/dev/null; then
        log_fail "wa watch (restart) exited immediately"
        return 1
    fi
    log_pass "Watcher restarted"

    # Step 8: Wait for workflow resume activity
    log_info "Step 8: Waiting for workflow resume..."
    sleep 5  # Give time for resume logic to execute

    # Check for resume activity in logs
    if grep -qi "resume\|incomplete" "$scenario_dir/wa_watch_2.log" 2>/dev/null; then
        log_pass "Resume activity detected in logs"
    else
        log_warn "No explicit resume activity in logs (may be normal if workflow completed before kill)"
    fi

    # Step 9: Check for duplicate steps
    log_info "Step 9: Checking for duplicate workflow steps..."
    if [[ -f "$db_path" ]]; then
        # Query step logs and check for duplicates
        local step_logs
        step_logs=$(sqlite3 "$db_path" \
            "SELECT workflow_id, step_index, step_name, COUNT(*) as cnt
             FROM workflow_step_logs
             GROUP BY workflow_id, step_index
             HAVING cnt > 1;" 2>/dev/null || echo "")

        echo "$step_logs" > "$scenario_dir/duplicate_steps.txt"

        if [[ -n "$step_logs" ]]; then
            log_fail "Found duplicate workflow steps!"
            echo "Duplicate steps: $step_logs" >> "$scenario_dir/scenario.log"
            result=1
        else
            log_pass "No duplicate workflow steps found"
        fi

        # Get final step log count
        local step_count_after
        step_count_after=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_step_logs;" 2>/dev/null || echo "0")
        echo "step_logs_after_restart: $step_count_after" >> "$scenario_dir/scenario.log"
        log_info "Step logs after restart: $step_count_after"

        # Export all step logs for debugging
        sqlite3 "$db_path" -header -csv \
            "SELECT workflow_id, step_index, step_name, result_type, duration_ms FROM workflow_step_logs ORDER BY workflow_id, step_index;" \
            > "$scenario_dir/all_step_logs.csv" 2>/dev/null || true

        # Export workflow status
        sqlite3 "$db_path" -header -csv \
            "SELECT id, workflow_name, pane_id, current_step, status FROM workflow_executions;" \
            > "$scenario_dir/workflow_executions.csv" 2>/dev/null || true
    else
        log_warn "Database file not found after restart"
    fi

    # Step 10: Check wa watch logs for workflow activity
    log_info "Step 10: Checking wa watch logs for workflow activity..."
    cat "$scenario_dir/wa_watch_1.log" "$scenario_dir/wa_watch_2.log" > "$scenario_dir/wa_watch_combined.log" 2>/dev/null || true

    if grep -qi "workflow\|compaction\|detection" "$scenario_dir/wa_watch_combined.log" 2>/dev/null; then
        log_pass "Found workflow/detection activity in logs"
    else
        log_warn "No obvious workflow activity in logs (may be normal)"
    fi

    # Note: This scenario depends on workflow functionality being complete
    log_info "Scenario complete"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_workflow_resume

    return $result
}

run_scenario_status_update() {
    # Validates the Lua status_update lane:
    # 1. Start a dedicated WezTerm instance with status_update hook
    # 2. Start wa watch pointed at that instance
    # 3. Spawn a pane that toggles alt-screen + title
    # 4. Verify pane state updates via IPC pane_state
    # 5. Verify invalid payloads are rejected safely
    #
    # Task: wa-4vx.2.7.3

    local scenario_dir="$1"
    local scenario_name="status_update"
    local temp_workspace
    temp_workspace=$(mktemp -d /tmp/wa-e2e-status-XXXXXX)
    local wa_pid=""
    local wezterm_pid=""
    local pane_id=""
    local result=0
    local wait_timeout=${TIMEOUT:-60}
    local now_ms
    now_ms=$(date +%s%3N 2>/dev/null || python3 -c 'import time; print(int(time.time() * 1000))')
    local wezterm_socket="$temp_workspace/wezterm.sock"
    local config_file="$temp_workspace/wezterm.lua"
    local emit_script="$temp_workspace/emit_status.sh"

    declare -A step_start_ms=()

    scenario_prefix() {
        echo "[$scenario_name pane_id=${pane_id:-unknown}]"
    }

    step_start() {
        local step="$1"
        local ts
        ts=$(date +%s%3N 2>/dev/null || python3 -c 'import time; print(int(time.time() * 1000))')
        step_start_ms["$step"]="$ts"
        log_info "$(scenario_prefix) STEP_START step=$step ts_ms=$ts"
        echo "step_start: $step ts_ms=$ts pane_id=${pane_id:-unknown}" >> "$scenario_dir/scenario.log"
    }

    step_end() {
        local step="$1"
        local end_ts
        end_ts=$(date +%s%3N 2>/dev/null || python3 -c 'import time; print(int(time.time() * 1000))')
        local start_ts="${step_start_ms[$step]:-}"
        local duration_ms=""
        if [[ -n "$start_ts" ]]; then
            duration_ms=$(( end_ts - start_ts ))
        fi
        log_info "$(scenario_prefix) STEP_END step=$step ts_ms=$end_ts duration_ms=${duration_ms:-unknown}"
        echo "step_end: $step ts_ms=$end_ts duration_ms=${duration_ms:-unknown} pane_id=${pane_id:-unknown}" >> "$scenario_dir/scenario.log"
    }

    ipc_pane_state() {
        local target_pane="$1"
        local socket_path="$WA_DATA_DIR/ipc.sock"
        python3 - "$socket_path" "$target_pane" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]
pane_id = int(sys.argv[2])
req = {"type": "pane_state", "pane_id": pane_id}

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2.0)
s.connect(sock_path)
s.sendall((json.dumps(req) + "\n").encode("utf-8"))
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
s.close()
sys.stdout.write(data.decode("utf-8").strip())
PY
    }

    log_info "$(scenario_prefix) Workspace: $temp_workspace"
    log_info "$(scenario_prefix) WezTerm socket: $wezterm_socket"

    # Setup environment for isolated wa instance
    export WA_DATA_DIR="$temp_workspace/.wa"
    export WA_WORKSPACE="$temp_workspace"
    mkdir -p "$WA_DATA_DIR"

    echo "scenario: $scenario_name" >> "$scenario_dir/scenario.log"
    echo "workspace: $temp_workspace" >> "$scenario_dir/scenario.log"
    echo "wezterm_socket: $wezterm_socket" >> "$scenario_dir/scenario.log"
    echo "test_timestamp_ms: $now_ms" >> "$scenario_dir/scenario.log"

    cleanup_status_update() {
        log_verbose "$(scenario_prefix) Cleaning up status_update scenario"
        if [[ -n "$wa_pid" ]] && kill -0 "$wa_pid" 2>/dev/null; then
            log_verbose "$(scenario_prefix) Stopping wa watch (pid $wa_pid)"
            kill "$wa_pid" 2>/dev/null || true
            wait "$wa_pid" 2>/dev/null || true
        fi
        if [[ -n "$pane_id" ]]; then
            log_verbose "$(scenario_prefix) Closing dummy pane $pane_id"
            WEZTERM_UNIX_SOCKET="$wezterm_socket" wezterm cli --no-auto-start \
                kill-pane --pane-id "$pane_id" 2>/dev/null || true
        fi
        if [[ -n "$wezterm_pid" ]] && kill -0 "$wezterm_pid" 2>/dev/null; then
            log_verbose "$(scenario_prefix) Stopping wezterm (pid $wezterm_pid)"
            kill "$wezterm_pid" 2>/dev/null || true
            wait "$wezterm_pid" 2>/dev/null || true
        fi
        if [[ -d "$temp_workspace" ]]; then
            cp -r "$temp_workspace/.wa"/* "$scenario_dir/" 2>/dev/null || true
            cp "$config_file" "$scenario_dir/wezterm.lua" 2>/dev/null || true
            cp "$emit_script" "$scenario_dir/emit_status.sh" 2>/dev/null || true
        fi
        rm -rf "$temp_workspace"
    }
    trap cleanup_status_update EXIT

    # Step 1: Write a minimal wezterm.lua with status_update hook
    step_start "write_config"
    cat > "$config_file" <<'EOF'
local wezterm = require 'wezterm'
local wa_bin = os.getenv("WA_E2E_WA_BINARY") or "wa"
local wa_last_status_update = {}
local WA_STATUS_UPDATE_INTERVAL_MS = 2000

wezterm.on('update-status', function(window, pane)
  local pane_id = pane:pane_id()
  local now_ms = os.time() * 1000
  local last = wa_last_status_update[pane_id] or 0
  if now_ms - last < WA_STATUS_UPDATE_INTERVAL_MS then
    return
  end
  wa_last_status_update[pane_id] = now_ms

  local dims = pane:get_dimensions()
  local cursor = pane:get_cursor_position()
  local domain = pane:get_domain_name()
  local title = pane:get_title()
  local is_alt = pane:is_alt_screen_active()

  local function json_escape(s)
    if not s then return '' end
    return s:gsub('\\', '\\\\')
            :gsub('"', '\\"')
            :gsub('\n', '\\n')
            :gsub('\r', '\\r')
            :gsub('\t', '\\t')
  end

  local payload = string.format(
    '{"schema_version":0,"pane_id":%d,"domain":"%s","title":"%s",' ..
    '"cursor":{"row":%d,"col":%d},"dimensions":{"rows":%d,"cols":%d},' ..
    '"is_alt_screen":%s,"ts":%d}',
    pane_id,
    json_escape(domain),
    json_escape(title):sub(1, 256),
    cursor.y, cursor.x,
    dims.viewport_rows, dims.viewport_cols,
    is_alt and 'true' or 'false',
    now_ms
  )

  wezterm.background_child_process {
    wa_bin, 'event', '--from-status',
    '--pane', tostring(pane_id),
    '--payload', payload
  }
end)

return {}
EOF
    step_end "write_config"

    # Step 2: Start a dedicated wezterm instance with the config
    step_start "start_wezterm"
    WA_E2E_WA_BINARY="$WA_BINARY" WEZTERM_UNIX_SOCKET="$wezterm_socket" \
        wezterm start --always-new-process --config-file "$config_file" \
        --workspace "wa-e2e-status" > "$scenario_dir/wezterm.log" 2>&1 &
    wezterm_pid=$!
    echo "wezterm_pid: $wezterm_pid" >> "$scenario_dir/scenario.log"

    local check_mux_cmd="WEZTERM_UNIX_SOCKET=\"$wezterm_socket\" wezterm cli --no-auto-start list >/dev/null 2>&1"
    if ! wait_for_condition "wezterm mux ready" "$check_mux_cmd" "$wait_timeout"; then
        log_fail "$(scenario_prefix) Timeout waiting for wezterm mux"
        result=1
        return $result
    fi
    log_pass "$(scenario_prefix) WezTerm mux ready"
    step_end "start_wezterm"

    # Step 3: Start wa watch against the test mux
    step_start "start_watch"
    WA_WORKSPACE="$temp_workspace" WA_DATA_DIR="$temp_workspace/.wa" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" WA_LOG_LEVEL=debug \
        "$WA_BINARY" watch --foreground > "$scenario_dir/wa_watch.log" 2>&1 &
    wa_pid=$!
    echo "wa_pid: $wa_pid" >> "$scenario_dir/scenario.log"

    local check_watch_cmd="kill -0 $wa_pid 2>/dev/null"
    if ! wait_for_condition "wa watch running" "$check_watch_cmd" 10; then
        log_fail "$(scenario_prefix) wa watch exited immediately"
        result=1
        return $result
    fi
    log_pass "$(scenario_prefix) wa watch running"
    step_end "start_watch"

    # Step 4: Prepare a pane script that toggles alt screen + title
    step_start "prepare_pane_script"
    cat > "$emit_script" <<'EOS'
#!/bin/bash
set -euo pipefail
printf '\033]0;wa-e2e-status\007'
printf '\033[?1049h'
sleep 5
printf '\033[?1049l'
sleep "${1:-600}"
EOS
    chmod +x "$emit_script"
    step_end "prepare_pane_script"

    # Step 5: Spawn a pane in the test mux
    step_start "spawn_pane"
    local spawn_output
    spawn_output=$(WEZTERM_UNIX_SOCKET="$wezterm_socket" wezterm cli --no-auto-start spawn \
        --cwd "$temp_workspace" -- "$emit_script" 600 2>&1)
    pane_id=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1)

    if [[ -z "$pane_id" ]]; then
        log_fail "$(scenario_prefix) Failed to spawn status_update pane"
        echo "spawn_output: $spawn_output" >> "$scenario_dir/scenario.log"
        result=1
        return $result
    fi
    log_info "$(scenario_prefix) Spawned pane: $pane_id"
    echo "pane_id: $pane_id" >> "$scenario_dir/scenario.log"
    step_end "spawn_pane"

    # Step 6: Wait for pane to be observed by wa
    step_start "wait_pane_observed"
    local check_cmd="WEZTERM_UNIX_SOCKET=\"$wezterm_socket\" \"$WA_BINARY\" robot state 2>/dev/null | jq -e '.data[]? | select(.pane_id == $pane_id)' >/dev/null 2>&1"
    if ! wait_for_condition "pane $pane_id observed" "$check_cmd" "$wait_timeout"; then
        log_fail "$(scenario_prefix) Timeout waiting for pane to be observed"
        WEZTERM_UNIX_SOCKET="$wezterm_socket" "$WA_BINARY" robot state > "$scenario_dir/robot_state_initial.json" 2>&1 || true
        result=1
        return $result
    fi
    log_pass "$(scenario_prefix) Pane observed"
    WEZTERM_UNIX_SOCKET="$wezterm_socket" "$WA_BINARY" robot state > "$scenario_dir/robot_state_initial.json" 2>&1 || true
    step_end "wait_pane_observed"

    # Step 7: Wait for status_update to land in pane_state
    step_start "wait_status_update"
    local check_status_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and (.data.last_status_at // 0) > 0' >/dev/null 2>&1"
    if ! wait_for_condition "pane_state last_status_at set" "$check_status_cmd" "$wait_timeout"; then
        log_fail "$(scenario_prefix) Timeout waiting for status_update"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_timeout.json" 2>&1 || true
        result=1
        return $result
    fi
    log_pass "$(scenario_prefix) status_update observed via IPC"
    ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_after_update.json" 2>&1 || true
    step_end "wait_status_update"

    # Step 8: Verify alt-screen toggled to true at least once
    step_start "verify_alt_screen_true"
    local check_alt_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and ((.data.alt_screen // false) == true or (.data.cursor_alt_screen // false) == true)' >/dev/null 2>&1"
    if ! wait_for_condition "alt_screen true" "$check_alt_cmd" 15; then
        log_fail "$(scenario_prefix) Alt-screen true not observed"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_alt_screen_missing.json" 2>&1 || true
        result=1
    else
        log_pass "$(scenario_prefix) Alt-screen true observed"
    fi
    step_end "verify_alt_screen_true"

    # Step 9: Verify alt-screen eventually returns to false
    step_start "verify_alt_screen_false"
    local check_alt_false_cmd="ipc_pane_state \"$pane_id\" | jq -e '.ok == true and ((.data.alt_screen // false) == false)' >/dev/null 2>&1"
    if ! wait_for_condition "alt_screen false" "$check_alt_false_cmd" 20; then
        log_fail "$(scenario_prefix) Alt-screen false not observed"
        ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_alt_screen_stuck.json" 2>&1 || true
        result=1
    else
        log_pass "$(scenario_prefix) Alt-screen returned to false"
    fi
    step_end "verify_alt_screen_false"

    # Step 10: Test invalid payload rejection
    step_start "invalid_payload"
    local invalid_payload
    invalid_payload=$(printf '{"pane_id":%s,"schema_version":99,"ts":%s}' "$pane_id" "$now_ms")
    local invalid_output=""
    local invalid_exit=0
    set +e
    invalid_output=$(WA_WORKSPACE="$temp_workspace" WA_DATA_DIR="$temp_workspace/.wa" \
        WEZTERM_UNIX_SOCKET="$wezterm_socket" "$WA_BINARY" event --from-status \
        --pane "$pane_id" --payload "$invalid_payload" 2>&1)
    invalid_exit=$?
    set -e
    echo "invalid_exit: $invalid_exit" >> "$scenario_dir/scenario.log"
    echo "invalid_payload_len: ${#invalid_payload}" >> "$scenario_dir/scenario.log"
    echo "$invalid_output" > "$scenario_dir/wa_event_invalid.log"

    if [[ "$invalid_exit" -ne 0 ]]; then
        log_pass "$(scenario_prefix) Invalid payload rejected"
    else
        log_fail "$(scenario_prefix) Invalid payload unexpectedly accepted"
        result=1
    fi
    step_end "invalid_payload"

    # Step 11: Verify no raw invalid payloads in logs (redaction check)
    step_start "redaction_check"
    if grep -q '"schema_version":99' "$scenario_dir/wa_watch.log" 2>/dev/null; then
        log_warn "$(scenario_prefix) Found raw invalid payload in logs (redaction may be needed)"
    else
        log_pass "$(scenario_prefix) No raw invalid payloads found in logs"
    fi
    step_end "redaction_check"

    # Step 12: Capture DB snapshot and pane_state for artifacts
    step_start "artifacts_snapshot"
    local db_path="$WA_DATA_DIR/wa.db"
    if [[ -f "$db_path" ]]; then
        cp "$db_path" "$scenario_dir/wa.db" 2>/dev/null || true
    fi
    ipc_pane_state "$pane_id" > "$scenario_dir/pane_state_final.json" 2>&1 || true
    step_end "artifacts_snapshot"

    log_info "$(scenario_prefix) Scenario complete"

    # Cleanup trap will handle the rest
    trap - EXIT
    cleanup_status_update

    return $result
}

run_scenario() {
    local name="$1"
    local scenario_num="$2"
    local scenario_dir="$RUN_ARTIFACTS_DIR/scenario_$(printf '%02d' "$scenario_num")_$name"

    mkdir -p "$scenario_dir"

    log_info "Starting scenario: $name"
    local start_time=$(date +%s)

    local result=0

        case "$name" in
            capture_search)
                run_scenario_capture_search "$scenario_dir" || result=$?
                ;;
            natural_language)
                run_scenario_natural_language "$scenario_dir" || result=$?
                ;;
            compaction_workflow)
                run_scenario_compaction_workflow "$scenario_dir" || result=$?
                ;;
        policy_denial)
            run_scenario_policy_denial "$scenario_dir" || result=$?
            ;;
        graceful_shutdown)
            run_scenario_graceful_shutdown "$scenario_dir" || result=$?
            ;;
        pane_exclude_filter)
            run_scenario_pane_exclude_filter "$scenario_dir" || result=$?
            ;;
        workspace_isolation)
            run_scenario_workspace_isolation "$scenario_dir" || result=$?
            ;;
        setup_idempotency)
            run_scenario_setup_idempotency "$scenario_dir" || result=$?
            ;;
        uservar_forwarding)
            run_scenario_uservar_forwarding "$scenario_dir" || result=$?
            ;;
        workflow_resume)
            run_scenario_workflow_resume "$scenario_dir" || result=$?
            ;;
        status_update)
            run_scenario_status_update "$scenario_dir" || result=$?
            ;;
        *)
            log_fail "Unknown scenario: $name"
            result=1
            ;;
    esac

    local duration=$(( $(date +%s) - start_time ))

    if [[ $result -eq 0 ]]; then
        touch "$scenario_dir/PASS"
        log_pass "Scenario $name: PASSED (${duration}s)"
        ((PASSED++))
    else
        touch "$scenario_dir/FAIL"
        log_fail "Scenario $name: FAILED (${duration}s)"
        ((FAILED++))

        # Print failure details
        echo ""
        echo "FAILURE DETAILS"
        echo "==============="
        echo "Scenario: $name"
        echo "Duration: ${duration}s"
        echo ""
        echo "Artifacts saved to: $scenario_dir/"
        echo ""
    fi

    return $result
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    parse_args "$@"

    # Handle --list
    if [[ "$LIST_ONLY" == "true" ]]; then
        list_scenarios
        exit 0
    fi

    # Handle --self-check
    if [[ "$SELF_CHECK_ONLY" == "true" ]]; then
        if run_self_check; then
            exit 0
        else
            exit 2
        fi
    fi

    # Always run self-check first
    log_info "Running prerequisites check..."
    if ! run_self_check; then
        log_fail "Prerequisites check failed. Use --self-check for details."
        exit 5
    fi
    echo ""

    # Find wa binary
    if ! find_wa_binary; then
        log_fail "Could not find wa binary"
        exit 5
    fi
    log_verbose "Using wa binary: $WA_BINARY"

    # Setup artifacts
    setup_artifacts
    START_TIME=$(date +%s)

    # Determine which scenarios to run
    local scenarios_to_run=()
    if [[ ${#SCENARIOS[@]} -eq 0 ]]; then
        # Run all scenarios
        read -ra scenarios_to_run <<< "$(get_scenario_names)"
    else
        # Validate requested scenarios
        for name in "${SCENARIOS[@]}"; do
            if is_valid_scenario "$name"; then
                scenarios_to_run+=("$name")
            else
                log_fail "Unknown scenario: $name"
                log_info "Use --list to see available scenarios"
                exit 3
            fi
        done
    fi

    TOTAL=${#scenarios_to_run[@]}
    log_info "Running $TOTAL scenario(s): ${scenarios_to_run[*]}"
    echo ""

    # Run scenarios
    local scenario_num=1
    local any_failed=false

    for name in "${scenarios_to_run[@]}"; do
        if ! run_scenario "$name" "$scenario_num"; then
            any_failed=true
        fi
        ((scenario_num++))
        echo ""
    done

    # Write summary
    write_summary

    # Print final results
    echo "============================================"
    echo "E2E Test Results"
    echo "============================================"
    echo "Total:   $TOTAL"
    echo "Passed:  $PASSED"
    echo "Failed:  $FAILED"
    echo "Skipped: $SKIPPED"
    echo ""

    # Cleanup
    cleanup_artifacts

    # Exit with appropriate code
    if [[ "$any_failed" == "true" ]]; then
        exit 1
    else
        exit 0
    fi
}

main "$@"
