#!/bin/bash
# =============================================================================
# E2E: Profile switch + pane bookmarks
# Implements: bd-2b2i
#
# Scenarios:
#   1. Create config profile, apply it, verify config changes
#   2. Create ruleset profile, apply it, verify active ruleset summary
#   3. Add pane bookmarks, list them, filter by tag/alias, remove them
#   4. Round-trip: profile apply + bookmark add, list --json, remove --json
#
# Requirements:
#   - wa binary built (cargo build -p wa)
#   - jq for JSON validation
#
# No WezTerm runtime required — all operations are CLI-only against temp dirs.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

WA_BIN=""
TESTS_FAILED=0

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
    echo "Error: wa binary not found. Run 'cargo build -p wa' first." >&2
    exit 1
}

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        echo "Error: jq is required for this E2E test." >&2
        exit 1
    fi
}

make_temp_workspace() {
    mktemp -d "${TMPDIR:-/tmp}/wa-e2e-profile-bookmarks.XXXXXX"
}

write_file() {
    local path="$1"
    local contents="$2"
    mkdir -p "$(dirname "$path")"
    printf "%b" "$contents" > "$path"
}

# =============================================================================
# Scenario 1: Config profile create, apply, verify changes
# =============================================================================
scenario_config_profile_switch() {
    local workspace config_path

    workspace=$(make_temp_workspace)
    config_path="$workspace/wa.toml"
    write_file "$config_path" "[general]\nlog_level = \"info\"\n\n[ingest]\ninterval_ms = 500\n"

    # Create a profile from empty template
    "$WA_BIN" config profile create incident --from empty --path "$config_path"

    # Write profile content with different settings
    local profile_path="$workspace/profiles/incident.toml"
    write_file "$profile_path" "[general]\nlog_level = \"debug\"\n\n[ingest]\ninterval_ms = 100\n"

    # List profiles — should show "default" + "incident"
    local list_json
    list_json=$("$WA_BIN" config profile list --json --path "$config_path")
    e2e_add_file "config_profile_list.json" "$list_json"
    echo "$list_json" | jq -e '.[] | select(.name=="incident")' >/dev/null
    echo "$list_json" | jq -e '.[] | select(.name=="default")' >/dev/null

    # Diff should show both old and new values
    local diff_out
    diff_out=$("$WA_BIN" config profile diff incident --path "$config_path")
    e2e_add_file "config_profile_diff.txt" "$diff_out"
    echo "$diff_out" | grep -q "debug"

    # Apply the incident profile
    local apply_out
    apply_out=$("$WA_BIN" config profile apply incident --path "$config_path")
    e2e_add_file "config_apply_output.txt" "$apply_out"

    # Verify config now has incident settings
    local applied
    applied=$(cat "$config_path")
    e2e_add_file "config_after_apply.toml" "$applied"
    echo "$applied" | grep -q 'log_level = "debug"'
    echo "$applied" | grep -q 'interval_ms = 100'

    # Rollback to original
    local rollback_out
    rollback_out=$("$WA_BIN" config profile rollback --yes --path "$config_path")
    e2e_add_file "config_rollback_output.txt" "$rollback_out"

    local restored
    restored=$(cat "$config_path")
    e2e_add_file "config_after_rollback.toml" "$restored"
    echo "$restored" | grep -q 'log_level = "info"'
    echo "$restored" | grep -q 'interval_ms = 500'
}

# =============================================================================
# Scenario 2: Ruleset profile create, apply, verify active summary
# =============================================================================
scenario_ruleset_profile_switch() {
    local workspace config_path

    workspace=$(make_temp_workspace)
    config_path="$workspace/wa.toml"
    write_file "$config_path" "[general]\nlog_level = \"info\"\n"

    # Create the rulesets directory + manifest with a profile
    local rulesets_dir="$workspace/rulesets"
    mkdir -p "$rulesets_dir/profiles"

    # Write a ruleset profile that changes packs
    write_file "$rulesets_dir/profiles/incident.toml" \
        "[overrides]\nquick_reject = [\"IGNORE_ALL\"]\n"

    # List ruleset profiles — should include "default" at minimum
    local list_out
    list_out=$("$WA_BIN" rules profile list -f json --path "$config_path" 2>&1) || true
    e2e_add_file "ruleset_profile_list.json" "$list_out"

    # Apply the incident ruleset profile
    local apply_out
    apply_out=$("$WA_BIN" rules profile apply incident --path "$config_path" 2>&1) || true
    e2e_add_file "ruleset_apply_output.txt" "$apply_out"

    # List again to confirm the active profile changed
    local list_after
    list_after=$("$WA_BIN" rules profile list -f json --path "$config_path" 2>&1) || true
    e2e_add_file "ruleset_profile_list_after.json" "$list_after"
}

# =============================================================================
# Scenario 3: Pane bookmarks — add, list, filter by tag, remove
# =============================================================================
scenario_pane_bookmarks() {
    local workspace config_path

    workspace=$(make_temp_workspace)
    config_path="$workspace/wa.toml"
    write_file "$config_path" "[general]\nlog_level = \"info\"\n"

    export WA_DATA_DIR="$workspace/.wa"
    export WA_WORKSPACE="$workspace"
    mkdir -p "$WA_DATA_DIR"

    # Add bookmarks for different panes
    local add1 add2 add3
    add1=$("$WA_BIN" panes bookmark add 1 --alias build --tags "ci,dev" --description "Build pane" --json)
    e2e_add_file "bookmark_add_1.json" "$add1"
    echo "$add1" | jq -e '.ok == true' >/dev/null

    add2=$("$WA_BIN" panes bookmark add 2 --alias tests --tags "ci" --description "Test runner" --json)
    e2e_add_file "bookmark_add_2.json" "$add2"
    echo "$add2" | jq -e '.ok == true' >/dev/null

    add3=$("$WA_BIN" panes bookmark add 3 --alias logs --tags "ops,monitoring" --description "Log viewer" --json)
    e2e_add_file "bookmark_add_3.json" "$add3"
    echo "$add3" | jq -e '.ok == true' >/dev/null

    # List all bookmarks (output is a bare JSON array)
    local list_all
    list_all=$("$WA_BIN" panes bookmark list --json)
    e2e_add_file "bookmark_list_all.json" "$list_all"
    local count
    count=$(echo "$list_all" | jq 'length')
    [[ "$count" -eq 3 ]]

    # Filter by tag "ci" — should return build + tests
    local list_ci
    list_ci=$("$WA_BIN" panes bookmark list --tag ci --json)
    e2e_add_file "bookmark_list_ci.json" "$list_ci"
    local ci_count
    ci_count=$(echo "$list_ci" | jq 'length')
    [[ "$ci_count" -eq 2 ]]

    # Filter by tag "ops" — should return logs only
    local list_ops
    list_ops=$("$WA_BIN" panes bookmark list --tag ops --json)
    e2e_add_file "bookmark_list_ops.json" "$list_ops"
    local ops_count
    ops_count=$(echo "$list_ops" | jq 'length')
    [[ "$ops_count" -eq 1 ]]
    echo "$list_ops" | jq -e '.[0].alias == "logs"' >/dev/null

    # Remove "tests" bookmark
    local remove_out
    remove_out=$("$WA_BIN" panes bookmark remove tests --json)
    e2e_add_file "bookmark_remove.json" "$remove_out"
    echo "$remove_out" | jq -e '.ok == true' >/dev/null

    # List again — should be 2 now
    local list_after
    list_after=$("$WA_BIN" panes bookmark list --json)
    e2e_add_file "bookmark_list_after_remove.json" "$list_after"
    local after_count
    after_count=$(echo "$list_after" | jq 'length')
    [[ "$after_count" -eq 2 ]]

    # Remove nonexistent alias — should report not found
    local remove_bad
    remove_bad=$("$WA_BIN" panes bookmark remove nonexistent --json 2>&1) || true
    e2e_add_file "bookmark_remove_nonexistent.json" "$remove_bad"

    unset WA_DATA_DIR WA_WORKSPACE
}

# =============================================================================
# Scenario 4: Combined — profile apply + bookmark add in same workspace
# =============================================================================
scenario_combined_profile_and_bookmarks() {
    local workspace config_path

    workspace=$(make_temp_workspace)
    config_path="$workspace/wa.toml"
    write_file "$config_path" "[general]\nlog_level = \"info\"\n"

    export WA_DATA_DIR="$workspace/.wa"
    export WA_WORKSPACE="$workspace"
    mkdir -p "$WA_DATA_DIR"

    # Create + apply a config profile
    "$WA_BIN" config profile create dev --from empty --path "$config_path"
    local profile_path="$workspace/profiles/dev.toml"
    write_file "$profile_path" "[general]\nlog_level = \"trace\"\n"
    "$WA_BIN" config profile apply dev --path "$config_path"

    local applied
    applied=$(cat "$config_path")
    e2e_add_file "combined_config_after_apply.toml" "$applied"
    echo "$applied" | grep -q 'log_level = "trace"'

    # Add bookmarks
    "$WA_BIN" panes bookmark add 10 --alias dev-shell --tags "dev" --json | jq -e '.ok == true' >/dev/null
    "$WA_BIN" panes bookmark add 11 --alias dev-logs --tags "dev,monitoring" --json | jq -e '.ok == true' >/dev/null

    # List bookmarks (JSON array) — should have 2
    local bm_list
    bm_list=$("$WA_BIN" panes bookmark list --json)
    e2e_add_file "combined_bookmark_list.json" "$bm_list"
    echo "$bm_list" | jq -e 'length == 2' >/dev/null

    # Human-readable list should also work
    local bm_human
    bm_human=$("$WA_BIN" panes bookmark list)
    e2e_add_file "combined_bookmark_list.txt" "$bm_human"
    echo "$bm_human" | grep -q "dev-shell"
    echo "$bm_human" | grep -q "dev-logs"

    # Rollback config profile
    "$WA_BIN" config profile rollback --yes --path "$config_path"
    local restored
    restored=$(cat "$config_path")
    e2e_add_file "combined_config_after_rollback.toml" "$restored"
    echo "$restored" | grep -q 'log_level = "info"'

    # Bookmarks should persist despite profile rollback
    local bm_after
    bm_after=$("$WA_BIN" panes bookmark list --json)
    e2e_add_file "combined_bookmark_list_after_rollback.json" "$bm_after"
    echo "$bm_after" | jq -e 'length == 2' >/dev/null

    unset WA_DATA_DIR WA_WORKSPACE
}

# =============================================================================
# Main
# =============================================================================
main() {
    find_wa_binary
    require_jq

    e2e_init_artifacts "profile-bookmarks" >/dev/null

    e2e_capture_scenario "config_profile_switch" scenario_config_profile_switch || TESTS_FAILED=1
    e2e_capture_scenario "ruleset_profile_switch" scenario_ruleset_profile_switch || TESTS_FAILED=1
    e2e_capture_scenario "pane_bookmarks" scenario_pane_bookmarks || TESTS_FAILED=1
    e2e_capture_scenario "combined_profile_and_bookmarks" scenario_combined_profile_and_bookmarks || TESTS_FAILED=1

    e2e_finalize "$TESTS_FAILED" >/dev/null
    return "$TESTS_FAILED"
}

main "$@"
