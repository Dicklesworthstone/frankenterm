#!/bin/bash
# =============================================================================
# E2E: Config profile create/list/apply/rollback
# Implements: bd-2ffe
#
# Purpose:
#   Validate CLI-driven profile management and rollback safety.
#
# Requirements:
#   - wa binary built (cargo build -p frankenterm)
#   - jq for JSON validation
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

FT_BIN=""
TESTS_FAILED=0

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

    echo "Error: wa binary not found. Run 'cargo build -p frankenterm' first." >&2
    exit 1
}

require_jq() {
    if ! command -v jq >/dev/null 2>&1; then
        echo "Error: jq is required for this E2E test." >&2
        exit 1
    fi
}

make_temp_workspace() {
    mktemp -d "${TMPDIR:-/tmp}/wa-e2e-profiles.XXXXXX"
}

write_file() {
    local path="$1"
    local contents="$2"
    mkdir -p "$(dirname "$path")"
    printf "%b" "$contents" > "$path"
}

scenario_profile_create_list_diff() {
    local workspace config_path profile_path list_out diff_out json_out
    workspace=$(make_temp_workspace)
    config_path="$workspace/ft.toml"

    write_file "$config_path" "[general]\nlog_level = \"info\"\n"

    "$FT_BIN" config profile create incident --from empty --path "$config_path"
    profile_path="$workspace/profiles/incident.toml"
    write_file "$profile_path" "[general]\nlog_level = \"debug\"\n"

    list_out=$("$FT_BIN" config profile list --path "$config_path")
    e2e_add_file "profile_list.txt" "$list_out"
    echo "$list_out" | grep -q "incident"

    diff_out=$("$FT_BIN" config profile diff incident --path "$config_path")
    e2e_add_file "profile_diff.txt" "$diff_out"
    echo "$diff_out" | grep -q "log_level = \"debug\""
    echo "$diff_out" | grep -q "log_level = \"info\""

    json_out=$("$FT_BIN" config profile list --json --path "$config_path")
    e2e_add_file "profile_list.json" "$json_out"
    echo "$json_out" | jq -e '.[] | select(.name=="incident")' >/dev/null
}

scenario_profile_apply_rollback() {
    local workspace config_path profile_path apply_out rollback_out
    workspace=$(make_temp_workspace)
    config_path="$workspace/ft.toml"

    write_file "$config_path" "[general]\nlog_level = \"info\"\n"
    profile_path="$workspace/profiles/incident.toml"
    write_file "$profile_path" "[general]\nlog_level = \"debug\"\n"

    apply_out=$("$FT_BIN" config profile apply incident --path "$config_path")
    e2e_add_file "apply_output.txt" "$apply_out"

    local applied
    applied=$(cat "$config_path")
    e2e_add_file "config_after_apply.toml" "$applied"
    echo "$applied" | grep -q "log_level = \"debug\""

    rollback_out=$("$FT_BIN" config profile rollback --yes --path "$config_path")
    e2e_add_file "rollback_output.txt" "$rollback_out"

    local restored
    restored=$(cat "$config_path")
    e2e_add_file "config_after_rollback.toml" "$restored"
    echo "$restored" | grep -q "log_level = \"info\""

    local backup_path
    backup_path="${config_path}.profile.bak"
    if [[ ! -f "$backup_path" ]]; then
        backup_path="${config_path%.*}.toml.profile.bak"
    fi
    [[ -f "$backup_path" ]]
}

main() {
    find_ft_binary
    require_jq

    e2e_init_artifacts "config-profiles" >/dev/null

    e2e_capture_scenario "profile_create_list_diff" scenario_profile_create_list_diff || TESTS_FAILED=1
    e2e_capture_scenario "profile_apply_rollback" scenario_profile_apply_rollback || TESTS_FAILED=1

    e2e_finalize "$TESTS_FAILED" >/dev/null
    return "$TESTS_FAILED"
}

main "$@"
