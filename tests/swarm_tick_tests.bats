#!/usr/bin/env bats
# Unit + golden tests for scripts/swarm-tick.sh
#
# Beads: ft-v5lz3.2.1 (initial suite), ft-v5lz3.2.7 (Linux portability).
# Platform: macOS + Linux. The harness stubs every external command
# (git/br/df/find/ls/du/ntm) so the script runs identically on both
# platforms. The default disk-volume path differs per OS but the
# tests pin DISK_VOL so output is deterministic.
#
# How it works:
#   - tests/fixtures/swarm-tick/_stubs/ contains a thin command stub for each
#     external dependency (git, br, df, find, ls, du, ntm).
#   - Each stub `cat`s a file from $FIXTURE_DIR (set per test).
#   - The bats setup() prepends _stubs/ to PATH and points FIXTURE_DIR at
#     the active fixture, so swarm-tick.sh runs hermetically.
#   - Output is compared to fixture's expected.json after scrubbing the
#     dynamic `ts` field via `jq -S '.ts="<scrubbed>"'`. `jq -S` sorts keys
#     so formatting differences are irrelevant.
#   - The agent-mail-fallback fixture is the schema-like compatibility gate
#     for red-mail Beads/git coordination. It intentionally pins dirty-path
#     risk categories and counts without adding a docs/json-schema robot file.
#   - expected_handoff.md pins the read-only red-mail Beads handoff comment
#     formatter; the script prints it but never posts it.
#
# Run:
#   bats tests/swarm_tick_tests.bats

setup() {
    TESTS_DIR="$(cd "$(dirname "${BATS_TEST_FILENAME}")" && pwd)"
    REPO_ROOT="$(cd "${TESTS_DIR}/.." && pwd)"
    SCRIPT="${REPO_ROOT}/scripts/swarm-tick.sh"
    STUBS_DIR="${TESTS_DIR}/fixtures/swarm-tick/_stubs"
    FIXTURES_ROOT="${TESTS_DIR}/fixtures/swarm-tick"

    [[ -x "$SCRIPT" ]] || chmod +x "$SCRIPT"

    TMP_DIR="$(mktemp -d /tmp/swarm-tick-test.XXXXXX)"
    LOG_FILE="${TMP_DIR}/test.log"
    : > "$LOG_FILE"

    case "$TMP_DIR" in
        /tmp/swarm-tick-test.*) : ;;
        *)
            echo "TMP_DIR=$TMP_DIR is not under /tmp/swarm-tick-test.* — refusing to run" >&2
            exit 1
            ;;
    esac
}

teardown() {
    if [[ -n "${TMP_DIR:-}" && -d "$TMP_DIR" ]]; then
        rm -rf "$TMP_DIR"
    fi
}

log_event() {
    local phase="$1"; shift
    local msg="$*"
    local stamp
    stamp="$(date +%s)"
    printf '{"ts":%s,"test":"%s","phase":"%s","msg":%s}\n' \
        "$stamp" "${BATS_TEST_NAME:-unknown}" "$phase" "$(jq -Rn --arg m "$msg" '$m')" \
        >> "$LOG_FILE" 2>/dev/null || true
}

# Run swarm-tick.sh against a named fixture and produce a normalized actual.json.
#   run_fixture <name>  →  $TMP_DIR/actual.json + $TMP_DIR/expected.json
# Both files are passed through `jq -S '.ts="<scrubbed>"'` so formatting and
# the dynamic `ts` are removed.
run_fixture() {
    local name="$1"
    local fixture="${FIXTURES_ROOT}/${name}"
    [[ -d "$fixture" ]] || { echo "missing fixture: $fixture" >&2; return 2; }

    log_event "setup" "fixture=$name"

    local raw="${TMP_DIR}/raw.json"
    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        bash "$SCRIPT" frankenterm > "$raw" 2>"${TMP_DIR}/stderr.log"

    log_event "run" "raw_bytes=$(wc -c < "$raw" | tr -d ' ')"

    jq -S '.ts="<scrubbed>"' "$raw" > "${TMP_DIR}/actual.json"
    jq -S . "${fixture}/expected.json" > "${TMP_DIR}/expected.json"
    log_event "compare" "actual=${TMP_DIR}/actual.json expected=${TMP_DIR}/expected.json"
}

run_agent_mail_fallback_fixture() {
    local fixture="${FIXTURES_ROOT}/agent-mail-fallback"
    [[ -d "$fixture" ]] || { echo "missing fixture: $fixture" >&2; return 2; }

    log_event "setup" "fixture=agent-mail-fallback"

    local raw="${TMP_DIR}/raw.json"
    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        FT_OPERATOR_NOW_ISO="2026-05-06T17:00:00Z" \
        FT_OPERATOR_NOW_EPOCH="1778086800" \
        bash "$SCRIPT" --agent-mail-fallback frankenterm > "$raw" 2>"${TMP_DIR}/stderr.log"

    log_event "run" "raw_bytes=$(wc -c < "$raw" | tr -d ' ')"

    jq -S . "$raw" > "${TMP_DIR}/actual.json"
    jq -S . "${fixture}/expected.json" > "${TMP_DIR}/expected.json"
    log_event "compare" "actual=${TMP_DIR}/actual.json expected=${TMP_DIR}/expected.json"
}

run_agent_mail_handoff_fixture() {
    local fixture="${FIXTURES_ROOT}/agent-mail-fallback"
    [[ -d "$fixture" ]] || { echo "missing fixture: $fixture" >&2; return 2; }

    log_event "setup" "fixture=agent-mail-fallback-handoff"

    local raw="${TMP_DIR}/actual_handoff.md"
    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        FT_OPERATOR_NOW_ISO="2026-05-06T17:00:00Z" \
        FT_OPERATOR_NOW_EPOCH="1778086800" \
        bash "$SCRIPT" --agent-mail-handoff \
            --bead ft-u45ni \
            --touched-path scripts/swarm-tick.sh \
            --touched-path docs/operator-runbook.md \
            --avoided-path crates/frankenterm/src/main.rs \
            --proof-command "bash -n scripts/swarm-tick.sh" \
            --proof-command "shellcheck scripts/swarm-tick.sh" \
            frankenterm > "$raw" 2>"${TMP_DIR}/stderr.log"

    cp "${fixture}/expected_handoff.md" "${TMP_DIR}/expected_handoff.md"
    log_event "compare" "actual=${TMP_DIR}/actual_handoff.md expected=${TMP_DIR}/expected_handoff.md"
}

# Assert that the running fixture's actual matches expected.
assert_match() {
    if ! diff -u "${TMP_DIR}/expected.json" "${TMP_DIR}/actual.json" > "${TMP_DIR}/diff.txt"; then
        log_event "diff" "$(cat "${TMP_DIR}/diff.txt")"
        echo "--- expected" >&2
        cat "${TMP_DIR}/expected.json" >&2
        echo "--- actual" >&2
        cat "${TMP_DIR}/actual.json" >&2
        echo "--- diff" >&2
        cat "${TMP_DIR}/diff.txt" >&2
        return 1
    fi
}

assert_handoff_match() {
    if ! diff -u "${TMP_DIR}/expected_handoff.md" "${TMP_DIR}/actual_handoff.md" > "${TMP_DIR}/diff.txt"; then
        log_event "diff" "$(cat "${TMP_DIR}/diff.txt")"
        echo "--- expected" >&2
        cat "${TMP_DIR}/expected_handoff.md" >&2
        echo "--- actual" >&2
        cat "${TMP_DIR}/actual_handoff.md" >&2
        echo "--- diff" >&2
        cat "${TMP_DIR}/diff.txt" >&2
        return 1
    fi
}

# ─── Golden tests ────────────────────────────────────────────────────────────

@test "healthy fixture: 12 open / 4 ready / 91% disk → matches golden" {
    run_fixture healthy
    assert_match
}

@test "empty fixture: no panes / no beads / 6% disk → matches golden" {
    run_fixture empty
    assert_match
}

@test "disk-pressure fixture: 96% disk / 7 stale / 11 total → matches golden" {
    run_fixture disk-pressure
    assert_match
}

@test "converged fixture: 0 ready / 0 in_progress / no recent commits → matches golden" {
    run_fixture converged
    assert_match
}

@test "agent-mail fallback fixture: red-mail marker / beads / dirty paths → matches golden" {
    run_agent_mail_fallback_fixture
    assert_match
    [[ "$(jq -r '.agent_mail.marker' "${TMP_DIR}/actual.json")" == *"retry once, do not repair/restart service"* ]]
    [[ "$(jq -r '.mode' "${TMP_DIR}/actual.json")" == "agent_mail_unavailable_beads_only" ]]
    [[ "$(jq -r '.git.risk_level' "${TMP_DIR}/actual.json")" == "high" ]]
    [[ "$(jq -r '.git.tracked_dirty_count' "${TMP_DIR}/actual.json")" == "2" ]]
    [[ "$(jq -r '.git.untracked_dirty_count' "${TMP_DIR}/actual.json")" == "2" ]]
    [[ "$(jq -r '.git.high_risk_count' "${TMP_DIR}/actual.json")" == "2" ]]
    [[ "$(jq -r '.git.conflict_hints[] | select(.path == ".beads/issues.jsonl") | .category' "${TMP_DIR}/actual.json")" == "shared_tracker" ]]
    [[ "$(jq -r '.git.conflict_hints[] | select(.path == ".stash_janitor_workspace/handoff_report.md") | .severity' "${TMP_DIR}/actual.json")" == "low" ]]
    [[ "$(jq -r '.beads.stale_reopen.default_action' "${TMP_DIR}/actual.json")" == "do_not_reopen" ]]
    [[ "$(jq -r '.beads.stale_reopen.threshold_seconds' "${TMP_DIR}/actual.json")" == "7200" ]]
    [[ "$(jq -r '.beads.stale_reopen.active_not_stale[] | select(.id == "ft-active1") | .recommendation' "${TMP_DIR}/actual.json")" == "do_not_reopen" ]]
    [[ "$(jq -r '.beads.stale_reopen.candidates[] | select(.id == "ft-stale1") | .recommendation' "${TMP_DIR}/actual.json")" == "status_check_before_reopen" ]]
    [[ "$(jq -r '.beads.stale_reopen.candidates[] | select(.id == "ft-stale1") | .reopen_command' "${TMP_DIR}/actual.json")" == 'br update ft-stale1 --status open --assignee "" --actor <agent>' ]]
    [[ "$(jq -r '.beads.stale_reopen.dirty_overlap_unknown[] | select(.path == "crates/frankenterm-core/src/storage.rs") | .recommendation' "${TMP_DIR}/actual.json")" == "do_not_reopen_related_beads_until_owner_clear" ]]
}

@test "agent-mail handoff fixture: reviewed beads comment block → matches golden" {
    run_agent_mail_handoff_fixture
    assert_handoff_match
    [[ "$(grep -c '^Touched paths:' "${TMP_DIR}/actual_handoff.md")" == "1" ]]
    [[ "$(grep -c '^Avoided paths:' "${TMP_DIR}/actual_handoff.md")" == "1" ]]
    [[ "$(grep -c '^Proof commands actually run:' "${TMP_DIR}/actual_handoff.md")" == "1" ]]
    grep -F "Sync chatter, transfer logs, and code presence alone are not proof." "${TMP_DIR}/actual_handoff.md" >/dev/null
}

# ─── Schema invariants (run on healthy fixture) ──────────────────────────────

@test "schema: top-level keys are exactly {ts, session, git, beads, disk, swarm, coordinator}" {
    run_fixture healthy
    keys="$(jq -r '. | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "beads,coordinator,disk,git,session,swarm,ts" ]]
}

@test "schema: beads has exactly {open, in_progress, blocked, ready}" {
    run_fixture healthy
    keys="$(jq -r '.beads | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "blocked,in_progress,open,ready" ]]
}

@test "schema: disk has exactly {data_avail, data_used_pct, stale_targets_12h, total_targets, targets_size_mb}" {
    run_fixture healthy
    keys="$(jq -r '.disk | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "data_avail,data_used_pct,stale_targets_12h,targets_size_mb,total_targets" ]]
}

@test "schema: git has exactly {commits_1h, commits_since_last_tick}" {
    run_fixture healthy
    keys="$(jq -r '.git | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "commits_1h,commits_since_last_tick" ]]
}

@test "schema: swarm has exactly {panes_count, agents}" {
    run_fixture healthy
    keys="$(jq -r '.swarm | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "agents,panes_count" ]]
}

@test "schema: coordinator has ntm robot-equivalent rollups" {
    run_fixture healthy
    keys="$(jq -r '.coordinator | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "auto_assign,conflicts,digest,mode,native_coordinator_available,status" ]]
    [[ "$(jq -r '.coordinator.mode' "${TMP_DIR}/actual.json")" == "ntm_robot_equivalents" ]]
    [[ "$(jq -r '.coordinator.native_coordinator_available | type' "${TMP_DIR}/actual.json")" == "boolean" ]]
}

@test "schema: each agent has exactly {idx, type, pane}" {
    run_fixture healthy
    keys="$(jq -r '.swarm.agents[0] | keys_unsorted | join(",")' "${TMP_DIR}/actual.json" | tr ',' '\n' | sort | tr '\n' ',' | sed 's/,$//')"
    [[ "$keys" == "idx,pane,type" ]]
}

@test "schema: numeric fields are JSON numbers, not strings" {
    run_fixture healthy
    [[ "$(jq -r '.beads.open | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.git.commits_1h | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.disk.stale_targets_12h | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.disk.total_targets | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.disk.targets_size_mb | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.swarm.panes_count | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.coordinator.status.total_agents | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.coordinator.digest.active_alerts | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.coordinator.conflicts.count | type' "${TMP_DIR}/actual.json")" == "number" ]]
    [[ "$(jq -r '.coordinator.auto_assign.recommendations | type' "${TMP_DIR}/actual.json")" == "number" ]]
}

# ─── Boundary cases ──────────────────────────────────────────────────────────

@test "boundary: ts is RFC3339 UTC ('Z'-suffixed)" {
    run_fixture healthy
    raw="${TMP_DIR}/raw.json"
    [[ -f "$raw" ]]
    ts="$(jq -r '.ts' "$raw")"
    [[ "$ts" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]
}

@test "boundary: session arg flows through to JSON output" {
    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="${FIXTURES_ROOT}/healthy" \
        REPO_ROOT="$REPO_ROOT" \
        bash "$SCRIPT" custom-session > "${TMP_DIR}/raw.json"
    [[ "$(jq -r '.session' "${TMP_DIR}/raw.json")" == "custom-session" ]]
    # When the session isn't in the fixture, fallback engages: empty agents.
    [[ "$(jq -r '.swarm.panes_count' "${TMP_DIR}/raw.json")" == "0" ]]
    [[ "$(jq -r '.swarm.agents | length' "${TMP_DIR}/raw.json")" == "0" ]]
}

@test "boundary: targets_size_mb is integer (not float / not string)" {
    run_fixture disk-pressure
    [[ "$(jq -r '.disk.targets_size_mb' "${TMP_DIR}/actual.json")" == "112640" ]]
    [[ "$(jq -r '.disk.targets_size_mb | type' "${TMP_DIR}/actual.json")" == "number" ]]
}

@test "boundary: 0 commits / 0 panes still produces valid JSON" {
    run_fixture empty
    # If the JSON were broken, jq above would have failed in run_fixture.
    [[ "$(jq -r '.git.commits_1h' "${TMP_DIR}/actual.json")" == "0" ]]
    [[ "$(jq -r '.swarm.panes_count' "${TMP_DIR}/actual.json")" == "0" ]]
}

@test "boundary: 96% disk + 7 stale dirs reflected in disk fields" {
    run_fixture disk-pressure
    [[ "$(jq -r '.disk.data_used_pct' "${TMP_DIR}/actual.json")" == "96%" ]]
    [[ "$(jq -r '.disk.stale_targets_12h' "${TMP_DIR}/actual.json")" == "7" ]]
    [[ "$(jq -r '.disk.total_targets' "${TMP_DIR}/actual.json")" == "11" ]]
}

@test "robustness: missing br fixture (DB-busy simulation) → ready falls back to 0" {
    fixture="${TMP_DIR}/db-busy"
    cp -R "${FIXTURES_ROOT}/healthy" "$fixture"
    # Replace br_ready.json with the error-shape `br` returns when DB is busy.
    cat > "${fixture}/br_ready.json" <<'EOF'
{"error":{"code":"DATABASE_ERROR","message":"Database error: database is busy"}}
EOF

    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        bash "$SCRIPT" frankenterm > "${TMP_DIR}/raw.json" 2>"${TMP_DIR}/stderr.log"
    # Error envelopes are not ready-item arrays; they must collapse to zero.
    [[ "$(jq -r '.beads.ready | type' "${TMP_DIR}/raw.json")" == "number" ]]
    [[ "$(jq -r '.beads.ready' "${TMP_DIR}/raw.json")" == "0" ]]
}

@test "robustness: empty session (panes_json empty) uses fallback object" {
    fixture="${TMP_DIR}/empty-session"
    cp -R "${FIXTURES_ROOT}/healthy" "$fixture"
    echo '{"sessions":[]}' > "${fixture}/ntm_robot_status.json"

    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        bash "$SCRIPT" frankenterm > "${TMP_DIR}/raw.json"
    # The script's fallback emits {panes_count:0, agents:[]} keeping output valid.
    jq . "${TMP_DIR}/raw.json" > /dev/null   # parse must succeed
    [[ "$(jq -r '.swarm.panes_count' "${TMP_DIR}/raw.json")" == "0" ]]
    [[ "$(jq -r '.swarm.agents | length' "${TMP_DIR}/raw.json")" == "0" ]]
}

@test "robustness: ntm returning nothing (command absent) still yields valid JSON" {
    fixture="${TMP_DIR}/no-ntm"
    cp -R "${FIXTURES_ROOT}/healthy" "$fixture"
    : > "${fixture}/ntm_robot_status.json"   # truly empty
    : > "${fixture}/ntm_robot_health.json"
    : > "${fixture}/ntm_robot_alerts.json"
    : > "${fixture}/ntm_robot_assign.json"
    : > "${fixture}/ntm_conflicts.json"

    PATH="${STUBS_DIR}:$PATH" \
        FIXTURE_DIR="$fixture" \
        REPO_ROOT="$REPO_ROOT" \
        bash "$SCRIPT" frankenterm > "${TMP_DIR}/raw.json"
    jq . "${TMP_DIR}/raw.json" > /dev/null
    [[ "$(jq -r '.swarm.panes_count' "${TMP_DIR}/raw.json")" == "0" ]]
    [[ "$(jq -r '.coordinator.status.total_agents' "${TMP_DIR}/raw.json")" == "0" ]]
    [[ "$(jq -r '.coordinator.auto_assign.recommendations' "${TMP_DIR}/raw.json")" == "0" ]]
}
