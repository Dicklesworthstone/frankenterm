#!/bin/bash
# =============================================================================
# E2E: Plan Preview + Workflow Execution Logs
# Implements: wa-upg.2.6
#
# Purpose:
#   Validate ActionPlan preview and workflow execution logging end-to-end.
#   This proves that:
#   - Dry-run produces a valid plan preview
#   - Workflow execution records step logs
#   - Failures reference specific step boundaries
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

# Binary path
WA_BIN=""
TEST_WORKSPACE=""
TEST_PANE_ID=""

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

# Run wa command with timeout, extracting JSON
run_wa_timeout() {
    local timeout_secs="${1:-5}"
    shift
    local raw_output
    raw_output=$(timeout "$timeout_secs" "$WA_BIN" "$@" 2>&1 || true)

    # Strip ANSI codes and extract JSON
    local stripped
    stripped=$(echo "$raw_output" | sed 's/\x1b\[[0-9;]*m//g')

    # Extract JSON from output (skip INFO lines)
    echo "$stripped" | awk '
        /^{/ { found=1 }
        found { print }
    '
}

# Check if JSON is valid
is_valid_json() {
    echo "$1" | jq . >/dev/null 2>&1
}

ensure_test_pane() {
    # Reuse an existing pane ID if still present.
    if [[ -n "$TEST_PANE_ID" ]]; then
        if wezterm cli list 2>/dev/null | grep -qE "^${TEST_PANE_ID}[[:space:]]"; then
            return 0
        fi
        TEST_PANE_ID=""
    fi

    # Prefer an already-available pane to avoid blocking on spawn.
    local existing_pane
    existing_pane=$(timeout 5 wezterm cli list 2>/dev/null | awk 'NR==1 {print $1}' || true)
    if [[ -n "$existing_pane" && "$existing_pane" =~ ^[0-9]+$ ]]; then
        TEST_PANE_ID="$existing_pane"
        e2e_add_file "pane_existing.txt" "pane_id=$TEST_PANE_ID"
        log_pass "using existing pane: $TEST_PANE_ID"
        return 0
    fi

    local marker="PLAN_WORKFLOW_E2E_$(date +%s%N)"
    local dummy_script="$PROJECT_ROOT/fixtures/e2e/dummy_print.sh"
    if [[ ! -x "$dummy_script" ]]; then
        log_skip "dummy script missing: $dummy_script"
        return 1
    fi

    local spawn_output
    spawn_output=$(timeout 10 wezterm cli spawn --cwd "$TEST_WORKSPACE" -- bash "$dummy_script" "$marker" 20 2>&1 || true)
    TEST_PANE_ID=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1 || true)

    if [[ -z "$TEST_PANE_ID" ]]; then
        e2e_add_file "pane_spawn_error.txt" "$spawn_output"
        log_skip "unable to spawn deterministic test pane (likely headless/no mux server)"
        return 1
    fi

    e2e_add_file "pane_spawn.txt" "pane_id=$TEST_PANE_ID marker=$marker"
    log_pass "spawned deterministic test pane: $TEST_PANE_ID"
    return 0
}

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    echo "========================================"
    echo "E2E: Plan Preview + Workflow Execution"
    echo "Implements: wa-upg.2.6"
    echo "========================================"

    # Initialize artifacts
    e2e_init_artifacts "plan-workflow" >/dev/null
    echo "[INFO] Artifacts directory: $E2E_RUN_DIR"

    log_test "Checking Prerequisites"

    # Find wa binary
    WA_BIN="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}/debug/wa"
    if [[ ! -x "$WA_BIN" ]]; then
        WA_BIN="$PROJECT_ROOT/target/debug/wa"
    fi

    if [[ ! -x "$WA_BIN" ]]; then
        echo "[INFO] Building wa binary..."
        cargo build -p wa 2>&1 | tail -5
    fi

    if [[ -x "$WA_BIN" ]]; then
        log_pass "wa binary found: $WA_BIN"
    else
        log_fail "wa binary not found"
        exit 1
    fi

    # Check jq
    if command -v jq &>/dev/null; then
        log_pass "jq available"
    else
        log_fail "jq not found"
        exit 1
    fi

    # Check sqlite3 for workflow/action-plan assertions
    if command -v sqlite3 &>/dev/null; then
        log_pass "sqlite3 available"
    else
        log_fail "sqlite3 not found"
        exit 1
    fi

    # Check wezterm for deterministic pane provisioning
    if command -v wezterm &>/dev/null; then
        log_pass "wezterm available"
    else
        log_fail "wezterm not found"
        exit 1
    fi

    # Create isolated workspace so this test never mutates the caller workspace
    TEST_WORKSPACE=$(mktemp -d /tmp/wa-e2e-plan-workflow-XXXXXX)
    export WA_WORKSPACE="$TEST_WORKSPACE"
    export WA_DATA_DIR="$TEST_WORKSPACE/.wa"
    mkdir -p "$WA_DATA_DIR"
    log_pass "isolated workspace initialized: $TEST_WORKSPACE"

    # Copy baseline config when present
    local baseline_config="$PROJECT_ROOT/fixtures/e2e/config_baseline.toml"
    if [[ -f "$baseline_config" ]]; then
        cp "$baseline_config" "$TEST_WORKSPACE/wa.toml"
        export WA_CONFIG="$TEST_WORKSPACE/wa.toml"
        log_pass "baseline config applied"
    else
        log_skip "baseline config not found; using defaults"
    fi
}

# ==============================================================================
# Test: Workflow List
# ==============================================================================

test_workflow_list() {
    log_test "Testing Workflow List"

    local output
    output=$(run_wa_timeout 5 robot workflow list)

    if ! is_valid_json "$output"; then
        log_fail "workflow list: not valid JSON"
        e2e_add_file "workflow_list_raw.txt" "$output"
        return
    fi

    e2e_add_json "workflow_list.json" "$output"

    # Check for expected fields
    local ok
    ok=$(echo "$output" | jq -r '.ok')
    if [[ "$ok" == "true" ]]; then
        log_pass "workflow list: successful response"
    else
        log_fail "workflow list: returned ok=false"
        return
    fi

    # Check for workflows
    local workflow_count
    workflow_count=$(echo "$output" | jq -r '.data.total // 0')
    if [[ "$workflow_count" -gt 0 ]]; then
        log_pass "workflow list: found $workflow_count workflows"
    else
        log_skip "workflow list: no workflows defined (expected for minimal setup)"
    fi

    # Check workflow structure
    local has_name has_desc
    has_name=$(echo "$output" | jq -r '.data.workflows[0].name // "missing"')
    has_desc=$(echo "$output" | jq -r '.data.workflows[0].description // "missing"')

    if [[ "$has_name" != "missing" && "$has_desc" != "missing" ]]; then
        log_pass "workflow list: workflows have name and description"
    else
        log_skip "workflow list: no workflows to validate structure"
    fi
}

# ==============================================================================
# Test: Plan Preview (Dry-Run)
# ==============================================================================

test_plan_preview() {
    log_test "Testing Plan Preview (Dry-Run)"

    if ! ensure_test_pane; then
        log_skip "plan preview: could not provision test pane"
        return
    fi

    echo "[INFO] Using pane $TEST_PANE_ID for plan preview test"

    # Run dry-run workflow
    local dry_run_output
    dry_run_output=$(run_wa_timeout 15 robot workflow run handle_compaction "$TEST_PANE_ID" --dry-run --format json)

    if ! is_valid_json "$dry_run_output"; then
        log_fail "plan preview: dry-run output not valid JSON"
        e2e_add_file "dry_run_raw.txt" "$dry_run_output"
        return
    fi

    e2e_add_json "plan_preview_dry_run.json" "$dry_run_output"

    # Check response structure
    local ok
    ok=$(echo "$dry_run_output" | jq -r '.ok')
    if [[ "$ok" == "true" ]]; then
        log_pass "plan preview: dry-run successful"
    else
        local error
        error=$(echo "$dry_run_output" | jq -r '.error // "unknown"')
        log_fail "plan preview: dry-run failed - $error"
        return
    fi

    # Check for expected_actions field (this is the plan preview)
    local has_actions
    has_actions=$(echo "$dry_run_output" | jq -r '.data.expected_actions | type // "null"')
    if [[ "$has_actions" == "array" ]]; then
        log_pass "plan preview: has expected_actions array"
    else
        log_fail "plan preview: missing expected_actions"
        return
    fi

    # Check action structure
    local action_count
    action_count=$(echo "$dry_run_output" | jq -r '.data.expected_actions | length')
    echo "[INFO] Plan has $action_count expected actions"

    if [[ "$action_count" -gt 0 ]]; then
        log_pass "plan preview: non-empty action list"
    else
        log_fail "plan preview: empty action list"
    fi

    # Verify each action has step, action_type, description
    local valid_actions=0
    for i in $(seq 0 $((action_count - 1))); do
        local step action_type desc
        step=$(echo "$dry_run_output" | jq -r ".data.expected_actions[$i].step // \"missing\"")
        action_type=$(echo "$dry_run_output" | jq -r ".data.expected_actions[$i].action_type // \"missing\"")
        desc=$(echo "$dry_run_output" | jq -r ".data.expected_actions[$i].description // \"missing\"")

        if [[ "$step" != "missing" && "$action_type" != "missing" && "$desc" != "missing" ]]; then
            ((valid_actions++)) || true
        fi
    done

    if [[ "$valid_actions" -eq "$action_count" ]]; then
        log_pass "plan preview: all actions have step/action_type/description"
    else
        log_fail "plan preview: $((action_count - valid_actions)) actions missing required fields"
    fi

    # Check policy_evaluation
    local has_policy
    has_policy=$(echo "$dry_run_output" | jq -r '.data.policy_evaluation.checks | type // "null"')
    if [[ "$has_policy" == "array" ]]; then
        log_pass "plan preview: has policy evaluation checks"
    else
        log_skip "plan preview: no policy evaluation in response"
    fi

    # Check target_resolution
    local has_target
    has_target=$(echo "$dry_run_output" | jq -r '.data.target_resolution.pane_id // "missing"')
    if [[ "$has_target" != "missing" ]]; then
        log_pass "plan preview: has target resolution"
    else
        log_skip "plan preview: no target resolution in response"
    fi
}

# ==============================================================================
# Test: Workflow Execution + Persisted Plan/Step Logs
# ==============================================================================

test_workflow_execution_logs() {
    log_test "Testing Workflow Execution + Persisted Plan/Step Logs"

    if ! ensure_test_pane; then
        log_skip "workflow execution logs: could not provision test pane"
        return
    fi

    local db_path="$WA_DATA_DIR/wa.db"
    local before_count=0
    if [[ -f "$db_path" ]]; then
        before_count=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_executions;" 2>/dev/null || echo "0")
    fi
    e2e_add_file "workflow_executions_before.txt" "$before_count"

    local run_output
    run_output=$(run_wa_timeout 20 robot workflow run handle_compaction "$TEST_PANE_ID" --format json)
    e2e_add_file "workflow_run_raw.txt" "$run_output"

    if is_valid_json "$run_output"; then
        e2e_add_json "workflow_run.json" "$run_output"
        log_pass "workflow run: valid JSON envelope"
    else
        log_fail "workflow run: invalid JSON output"
        return
    fi

    local after_count
    after_count=$(sqlite3 "$db_path" "SELECT COUNT(*) FROM workflow_executions;" 2>/dev/null || echo "0")
    e2e_add_file "workflow_executions_after.txt" "$after_count"
    if [[ "$after_count" -gt "$before_count" ]]; then
        log_pass "workflow execution persisted to DB ($before_count -> $after_count)"
    else
        log_fail "workflow execution not persisted to DB ($before_count -> $after_count)"
        return
    fi

    local execution_id
    execution_id=$(sqlite3 "$db_path" "SELECT id FROM workflow_executions ORDER BY started_at DESC LIMIT 1;" 2>/dev/null || true)
    if [[ -z "$execution_id" ]]; then
        log_fail "unable to resolve latest workflow execution id from DB"
        return
    fi
    e2e_add_file "workflow_execution_id.txt" "$execution_id"
    log_pass "resolved latest execution id: $execution_id"

    local status_output
    status_output=$(run_wa_timeout 15 robot workflow status "$execution_id" --verbose --format json)
    if ! is_valid_json "$status_output"; then
        log_fail "workflow status: invalid JSON"
        e2e_add_file "workflow_status_verbose_raw.txt" "$status_output"
        return
    fi
    e2e_add_json "workflow_status_verbose.json" "$status_output"

    if echo "$status_output" | jq -e '.ok == true' >/dev/null 2>&1; then
        log_pass "workflow status: ok=true for persisted execution"
    else
        log_fail "workflow status: expected ok=true"
        return
    fi

    if echo "$status_output" | jq -e '.data.action_plan.plan_id | type == "string"' >/dev/null 2>&1 \
        && echo "$status_output" | jq -e '.data.action_plan.plan_hash | type == "string"' >/dev/null 2>&1; then
        log_pass "workflow status: action_plan metadata persisted"
    else
        log_fail "workflow status: missing action_plan metadata"
        return
    fi

    local step_log_count
    step_log_count=$(echo "$status_output" | jq -r '.data.step_logs | length // 0')
    e2e_add_file "workflow_step_log_count.txt" "$step_log_count"
    if [[ "$step_log_count" -gt 0 ]]; then
        log_pass "workflow status: step logs captured ($step_log_count)"
    else
        log_fail "workflow status: no step logs captured"
        return
    fi

    local status_value
    status_value=$(echo "$status_output" | jq -r '.data.status // "unknown"')
    local boundary_evidence
    boundary_evidence=$(echo "$status_output" | jq -r '
      (.data.current_step != null) or
      ((.data.step_logs // []) | any(.error_code != null)) or
      ((.data.step_logs // []) | any((.result_type // "") != "Done"))
    ')
    if [[ "$status_value" == "completed" ]]; then
        log_pass "workflow completed; status and step logs are queryable"
    elif [[ "$boundary_evidence" == "true" ]]; then
        log_pass "workflow failure surfaced with step-boundary evidence"
    else
        log_fail "workflow failure missing step-boundary evidence"
        return
    fi

    local summary
    summary=$(cat <<EOF
execution_id: $execution_id
workflow_name: handle_compaction
status: $status_value
step_logs: $step_log_count
plan_id: $(echo "$status_output" | jq -r '.data.action_plan.plan_id')
plan_hash: $(echo "$status_output" | jq -r '.data.action_plan.plan_hash')
EOF
)
    e2e_add_file "workflow_execution_summary.txt" "$summary"
    log_pass "workflow execution summary artifact written"
}

# ==============================================================================
# Test: Workflow Status
# ==============================================================================

test_workflow_status() {
    log_test "Testing Workflow Status API"

    # Check status command works with no active workflows
    local status_output
    status_output=$(run_wa_timeout 5 robot workflow status --active)

    if ! is_valid_json "$status_output"; then
        log_fail "workflow status: not valid JSON"
        e2e_add_file "workflow_status_raw.txt" "$status_output"
        return
    fi

    e2e_add_json "workflow_status.json" "$status_output"

    local ok
    ok=$(echo "$status_output" | jq -r '.ok')
    if [[ "$ok" == "true" ]]; then
        log_pass "workflow status: successful response"
    else
        # May be expected if no workflow system running
        local error_code
        error_code=$(echo "$status_output" | jq -r '.error_code // "unknown"')
        log_skip "workflow status: $error_code (expected without watcher)"
    fi
}

# ==============================================================================
# Test: Events with Workflow Preview
# ==============================================================================

test_events_workflow() {
    log_test "Testing Events with Workflow Preview"

    # Check events command with would-handle flag
    local events_output
    events_output=$(run_wa_timeout 5 robot events --would-handle)

    if ! is_valid_json "$events_output"; then
        log_fail "events --would-handle: not valid JSON"
        e2e_add_file "events_would_handle_raw.txt" "$events_output"
        return
    fi

    e2e_add_json "events_would_handle.json" "$events_output"

    local ok
    ok=$(echo "$events_output" | jq -r '.ok')
    if [[ "$ok" == "true" ]]; then
        log_pass "events --would-handle: successful response"
    else
        log_skip "events --would-handle: failed (expected without watcher)"
    fi

    # Check for workflow_suggestion field in events (if any events exist)
    local events_count
    events_count=$(echo "$events_output" | jq -r '.data.events | length // 0')

    if [[ "$events_count" -gt 0 ]]; then
        # Check if any event has workflow_suggestion
        local has_suggestion
        has_suggestion=$(echo "$events_output" | jq -r '[.data.events[] | select(.workflow_suggestion != null)] | length')
        if [[ "$has_suggestion" -gt 0 ]]; then
            log_pass "events: found $has_suggestion events with workflow_suggestion"
        else
            log_skip "events: no events have workflow_suggestion (expected for test data)"
        fi
    else
        log_skip "events: no events to check for workflow suggestions"
    fi
}

# ==============================================================================
# Test: Workflow JSON Schema Compliance
# ==============================================================================

test_workflow_schemas() {
    log_test "Testing Workflow Response Schemas"

    local schema_dir="$PROJECT_ROOT/docs/json-schema"

    if ! command -v jsonschema >/dev/null 2>&1; then
        log_skip "jsonschema not installed; skipping schema validation checks"
        return
    fi

    # Check workflow-list schema
    local list_schema="$schema_dir/wa-robot-workflow-list.json"
    if [[ -f "$list_schema" ]]; then
        local list_output
        list_output=$(run_wa_timeout 5 robot workflow list)

        if is_valid_json "$list_output"; then
            local data
            data=$(echo "$list_output" | jq '.data')

            local temp_file
            temp_file=$(mktemp)
            echo "$data" > "$temp_file"

            if jsonschema -i "$temp_file" "$list_schema" 2>/dev/null; then
                log_pass "workflow list: schema valid"
            else
                log_fail "workflow list: schema validation failed"
            fi
            rm -f "$temp_file"
        else
            log_skip "workflow list: invalid JSON, skipping schema check"
        fi
    else
        log_skip "workflow list: schema not found"
    fi

    # Check workflow-status schema
    local status_schema="$schema_dir/wa-robot-workflow-status.json"
    if [[ -f "$status_schema" ]]; then
        log_skip "workflow status schema: requires execution_id to test"
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
    test_workflow_list
    test_plan_preview
    test_workflow_execution_logs
    test_workflow_status
    test_events_workflow
    test_workflow_schemas
    print_summary
}

main "$@"
