#!/usr/bin/env bats
# Tests for scripts/filter-clippy-owned-files.sh.
#
# Run:
#   bats tests/clippy_owned_filter_tests.bats

setup() {
    TESTS_DIR="$(cd "$(dirname "${BATS_TEST_FILENAME}")" && pwd)"
    REPO_ROOT="$(cd "${TESTS_DIR}/.." && pwd)"
    SCRIPT="${REPO_ROOT}/scripts/filter-clippy-owned-files.sh"
    FIXTURE_DIR="${TESTS_DIR}/fixtures/clippy-owned-filter"
}

@test "filters cargo JSONL diagnostics to owned files and preserves cargo status" {
    run bash "$SCRIPT" \
        --cargo-status 101 \
        --owned-file crates/frankenterm-core/src/color_management.rs \
        --owned-file crates/frankenterm-core/src/replay_fixture_harvest.rs \
        --input "${FIXTURE_DIR}/mixed.jsonl"

    [ "$status" -eq 0 ]
    jq . <<<"$output" >/dev/null
    [ "$(jq -r '.cargo_status' <<<"$output")" = "101" ]
    [ "$(jq -r '.full_command_failed' <<<"$output")" = "true" ]
    [ "$(jq -r '.workspace_green' <<<"$output")" = "false" ]
    [ "$(jq -r '.owned_error_count' <<<"$output")" = "1" ]
    [ "$(jq -r '.owned_warning_count' <<<"$output")" = "1" ]
    [ "$(jq -r '.attribution_verdict' <<<"$output")" = "owned_errors" ]
    [ "$(jq -r '.owned_diagnostics[].files[]' <<<"$output" | grep -c '^crates/frankenterm-core/src/storage.rs$')" -eq 0 ]
}

@test "reports owned files clean without claiming workspace clippy green" {
    run bash "$SCRIPT" \
        --cargo-status 101 \
        --owned-file crates/frankenterm-core/src/color_management.rs \
        --input "${FIXTURE_DIR}/unrelated.jsonl"

    [ "$status" -eq 0 ]
    [ "$(jq -r '.workspace_green' <<<"$output")" = "false" ]
    [ "$(jq -r '.owned_error_count' <<<"$output")" = "0" ]
    [ "$(jq -r '.owned_diagnostic_count' <<<"$output")" = "0" ]
    [ "$(jq -r '.attribution_verdict' <<<"$output")" = "owned_files_clean" ]
    [[ "$(jq -r '.proof_note' <<<"$output")" == *"not workspace green"* ]]
}

@test "text format keeps the full command status visible" {
    run bash "$SCRIPT" \
        --cargo-status 101 \
        --owned-file crates/frankenterm-core/src/color_management.rs \
        --input "${FIXTURE_DIR}/unrelated.jsonl" \
        --format text

    [ "$status" -eq 0 ]
    [[ "$output" == *"cargo_status=101"* ]]
    [[ "$output" == *"workspace_green=false"* ]]
    [[ "$output" == *"owned_error_count=0"* ]]
    [[ "$output" == *"attribution_verdict=owned_files_clean"* ]]
}

@test "normalizes dot-prefixed and repo-root absolute diagnostic paths" {
    fixture="${BATS_TEST_TMPDIR}/absolute-and-dot.jsonl"
    printf '{"reason":"compiler-message","message":{"level":"error","message":"dot path","spans":[{"file_name":"./crates/frankenterm-core/src/color_management.rs"}],"children":[]}}\n' > "$fixture"
    printf '{"reason":"compiler-message","message":{"level":"warning","message":"absolute repo path","spans":[{"file_name":"%s/crates/frankenterm-core/src/replay_fixture_harvest.rs"}],"children":[]}}\n' "$REPO_ROOT" >> "$fixture"
    printf '{"reason":"compiler-message","message":{"level":"error","message":"unrelated absolute path","spans":[{"file_name":"/tmp/not-the-repo/crates/frankenterm-core/src/color_management.rs"}],"children":[]}}\n' >> "$fixture"

    run bash "$SCRIPT" \
        --cargo-status 101 \
        --repo-root "$REPO_ROOT" \
        --owned-file crates/frankenterm-core/src/color_management.rs \
        --owned-file crates/frankenterm-core/src/replay_fixture_harvest.rs \
        --input "$fixture"

    [ "$status" -eq 0 ]
    [ "$(jq -r '.owned_diagnostic_count' <<<"$output")" = "2" ]
    [ "$(jq -r '.owned_error_count' <<<"$output")" = "1" ]
    [ "$(jq -r '.owned_warning_count' <<<"$output")" = "1" ]
    [ "$(jq -r '.owned_diagnostics[].message' <<<"$output" | grep -c '^unrelated absolute path$')" -eq 0 ]
}

@test "requires explicit cargo status" {
    run bash "$SCRIPT" \
        --owned-file crates/frankenterm-core/src/color_management.rs \
        --input "${FIXTURE_DIR}/unrelated.jsonl"

    [ "$status" -eq 64 ]
    [[ "$output" == *"--cargo-status must be a non-negative integer"* ]]
}
