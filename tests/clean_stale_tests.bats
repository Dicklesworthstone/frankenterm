#!/usr/bin/env bats
# Unit tests for scripts/clean-stale-targets.sh
#
# Beads: ft-v5lz3.2.2 (initial suite), ft-v5lz3.2.6 (Linux portability).
# Platform: macOS + Linux. mtime is read via the script's
# read_mtime_seconds (uname-branched stat). Test fixtures use
# `mtime_minutes_ago` (uname-branched date arithmetic) below.
#
# Run:
#   bats tests/clean_stale_tests.bats

# Compute a `touch -t`-compatible timestamp N minutes in the past.
# BSD date supports `date -v-NM`; GNU date wants `date -d "N min ago"`.
mtime_minutes_ago() {
    local age_min="$1"
    case "$(uname -s)" in
        Darwin)
            date -v-"${age_min}"M +%Y%m%d%H%M.%S
            ;;
        *)
            date -d "${age_min} minutes ago" +%Y%m%d%H%M.%S
            ;;
    esac
}

setup() {
    # Resolve repo root from this test file.
    TESTS_DIR="$(cd "$(dirname "${BATS_TEST_FILENAME}")" && pwd)"
    REPO_ROOT="$(cd "${TESTS_DIR}/.." && pwd)"
    SCRIPT="${REPO_ROOT}/scripts/clean-stale-targets.sh"

    [[ -x "$SCRIPT" ]] || chmod +x "$SCRIPT"

    # Hermetic per-test temp dir; never leaks into real /tmp/ft-*-target.
    # Use an explicit template so the dir lands in /tmp (macOS `mktemp -d -t`
    # otherwise picks /var/folders/...).
    TEST_DIR="$(mktemp -d /tmp/clean-stale-test.XXXXXX)"
    export TARGET_GLOB="${TEST_DIR}/ft-*-target"
    export FT_OPERATOR_LOCK_DIR="${TEST_DIR}/operator.lock"

    # JSON-line trace for CI artifact upload.
    LOG_FILE="${TEST_DIR}/test.log"
    : > "$LOG_FILE"

    # Sanity: refuse to run if the test dir is anywhere that could reach a
    # real agent's /tmp/ft-*-target cache.
    case "$TEST_DIR" in
        /tmp/clean-stale-test.*) : ;;
        *)
            echo "TEST_DIR=$TEST_DIR is not under /tmp/clean-stale-test.* — refusing to run" >&2
            exit 1
            ;;
    esac
}

teardown() {
    if [[ -n "${TEST_DIR:-}" && -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR"
    fi
}

# Logs a JSON-line event to the per-test log file (and to stderr if running
# with `bats --verbose-run`).
log_event() {
    local phase="$1"; shift
    local msg="$*"
    local stamp
    stamp="$(date +%s)"
    printf '{"ts":%s,"test":"%s","phase":"%s","msg":%s}\n' \
        "$stamp" "${BATS_TEST_NAME:-unknown}" "$phase" "$(jq -Rn --arg m "$msg" '$m')" \
        >> "$LOG_FILE" 2>/dev/null || true
}

# Make a fake target dir with a controlled mtime.
#   make_target <name> <minutes_old>
make_target() {
    local name="$1"
    local age_min="$2"
    local d="${TEST_DIR}/${name}"
    mkdir -p "$d"
    # `touch -t` takes [[CC]YY]MMDDhhmm[.SS] on both BSD and GNU.
    local stamp
    stamp="$(mtime_minutes_ago "$age_min")"
    touch -t "$stamp" "$d"
    log_event "fixture" "${name} aged ${age_min}m"
    echo "$d"
}

# Helper: count remaining target dirs in the test dir.
count_remaining() {
    # shellcheck disable=SC2086
    set -- ${TARGET_GLOB}
    local n=0
    for p in "$@"; do
        [[ -d "$p" ]] && n=$((n + 1))
    done
    echo "$n"
}

@test "no candidates: glob matches nothing → exit 0, cleaned 0" {
    log_event "act" "running with empty fixture"
    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs"* ]]
}

@test "all dirs newer than threshold → 0 removed" {
    make_target "ft-fresh-a-target" 30   >/dev/null
    make_target "ft-fresh-b-target" 60   >/dev/null

    run "$SCRIPT" 12   # threshold = 720 min
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs"* ]]
    [[ "$output" != *"removed "* ]]
    [ "$(count_remaining)" -eq 2 ]
}

@test "all dirs older than threshold → all removed" {
    make_target "ft-stale-a-target" 1500 >/dev/null
    make_target "ft-stale-b-target" 2000 >/dev/null

    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"removed "*"ft-stale-a-target"* ]]
    [[ "$output" == *"removed "*"ft-stale-b-target"* ]]
    [[ "$output" == *"cleaned 2 dirs"* ]]
    [ "$(count_remaining)" -eq 0 ]
}

@test "mixed ages → only old dirs removed" {
    make_target "ft-fresh-a-target" 30   >/dev/null
    make_target "ft-stale-b-target" 1500 >/dev/null
    make_target "ft-fresh-c-target" 60   >/dev/null
    make_target "ft-stale-d-target" 2000 >/dev/null

    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 2 dirs"* ]]
    [[ "$output" == *"ft-stale-b-target"* ]]
    [[ "$output" == *"ft-stale-d-target"* ]]
    [[ "$output" != *"ft-fresh-a-target"* ]]
    [[ "$output" != *"ft-fresh-c-target"* ]]
    [ "$(count_remaining)" -eq 2 ]
}

@test "threshold=0 → all dirs are stale" {
    make_target "ft-recent-target" 5    >/dev/null
    make_target "ft-old-target"    100  >/dev/null

    run "$SCRIPT" 0
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 2 dirs"* ]]
    [ "$(count_remaining)" -eq 0 ]
}

@test "very large threshold → nothing stale even if old" {
    make_target "ft-stale-target" 5000 >/dev/null   # ~83 hours
    run "$SCRIPT" 1000   # 60000 min threshold
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs"* ]]
    [ "$(count_remaining)" -eq 1 ]
}

@test "default hours (no arg) = 12" {
    make_target "ft-stale-target" 800  >/dev/null   # > 720 min
    run "$SCRIPT"
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
}

@test "boundary: exactly threshold minutes is NOT stale" {
    # threshold_min = 720; age = 720 → 720 > 720 is false → keep.
    make_target "ft-edge-target" 720 >/dev/null
    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs"* ]]
    [ "$(count_remaining)" -eq 1 ]
}

@test "boundary: threshold + 1 minute IS stale" {
    make_target "ft-edge-target" 721 >/dev/null
    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
    [ "$(count_remaining)" -eq 0 ]
}

@test "empty stale dir → removed" {
    d="$(make_target "ft-empty-target" 1500)"
    [ -d "$d" ]
    [ -z "$(ls -A "$d")" ]
    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
}

@test "non-empty stale dir with content → removed recursively" {
    d="$(make_target "ft-nonempty-target" 1500)"
    mkdir -p "$d/release/build"
    touch "$d/release/build/foo.o"
    # touch must NOT change the parent dir's mtime back to "now".
    stamp="$(mtime_minutes_ago 1500)"
    touch -t "$stamp" "$d"
    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
    [ ! -d "$d" ]
}

@test "--dry-run: stale dirs are NOT removed, summary reports would-have-cleaned" {
    make_target "ft-stale-a-target" 1500 >/dev/null
    make_target "ft-stale-b-target" 2000 >/dev/null

    run "$SCRIPT" --dry-run 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"[dry-run] would-remove"*"ft-stale-a-target"* ]]
    [[ "$output" == *"[dry-run] would-remove"*"ft-stale-b-target"* ]]
    [[ "$output" == *"cleaned 0 dirs (would have cleaned 2, skipped 0)"* ]]
    [[ "$output" != *"removed /tmp"* ]]   # no real-mode "removed " line
    [ "$(count_remaining)" -eq 2 ]
}

@test "DRY_RUN=1 env var: equivalent to --dry-run" {
    make_target "ft-stale-target" 1500 >/dev/null
    DRY_RUN=1 run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"[dry-run] would-remove"* ]]
    [[ "$output" == *"cleaned 0 dirs (would have cleaned 1, skipped 0)"* ]]
    [ "$(count_remaining)" -eq 1 ]
}

@test "--dry-run with no candidates → exit 0, would-have-cleaned 0" {
    run "$SCRIPT" --dry-run 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs (would have cleaned 0, skipped 0)"* ]]
}

@test "--dry-run with hours arg in either order" {
    make_target "ft-stale-target" 1500 >/dev/null

    run "$SCRIPT" --dry-run 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"would have cleaned 1"* ]]

    run "$SCRIPT" 12 --dry-run
    [ "$status" -eq 0 ]
    [[ "$output" == *"would have cleaned 1"* ]]
}

@test "--threshold-hours accepts documented runbook dry-run form" {
    make_target "ft-stale-target" 1500 >/dev/null
    make_target "ft-fresh-target" 30 >/dev/null

    run "$SCRIPT" --dry-run --threshold-hours 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"[dry-run] would-remove"*"ft-stale-target"* ]]
    [[ "$output" != *"ft-fresh-target"* ]]
    [[ "$output" == *"cleaned 0 dirs (would have cleaned 1, skipped 0)"* ]]
    [ "$(count_remaining)" -eq 2 ]
}

@test "--threshold-hours accepts equals form" {
    make_target "ft-stale-target" 1500 >/dev/null

    run "$SCRIPT" --dry-run --threshold-hours=12
    [ "$status" -eq 0 ]
    [[ "$output" == *"[dry-run] would-remove"*"ft-stale-target"* ]]
    [[ "$output" == *"would have cleaned 1"* ]]
    [ "$(count_remaining)" -eq 1 ]
}

@test "--threshold-hours missing value rejects" {
    run "$SCRIPT" --dry-run --threshold-hours
    [ "$status" -eq 2 ]
    [[ "$output" == *"missing value for --threshold-hours"* ]]

    run "$SCRIPT" --threshold-hours --dry-run
    [ "$status" -eq 2 ]
    [[ "$output" == *"missing value for --threshold-hours"* ]]
}

@test "--threshold-hours non-numeric value rejects" {
    run "$SCRIPT" --threshold-hours abc
    [ "$status" -eq 2 ]
    [[ "$output" == *"hours must be a non-negative integer"* ]]
}

@test "--threshold-hours rejects duplicate threshold values" {
    run "$SCRIPT" --threshold-hours 12 24
    [ "$status" -eq 2 ]
    [[ "$output" == *"threshold hours specified more than once"* ]]

    run "$SCRIPT" 12 --threshold-hours 24
    [ "$status" -eq 2 ]
    [[ "$output" == *"threshold hours specified more than once"* ]]
}

@test "race: dir disappears between scan and remove → script still exits 0" {
    d="$(make_target "ft-race-target" 1500)"

    # Pre-emptively delete the dir; the script should silently tolerate that.
    rm -rf "$d"

    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 0 dirs"* ]]
}

@test "unknown flag → exit 2 with helpful message" {
    run "$SCRIPT" --bogus
    [ "$status" -eq 2 ]
    [[ "$output" == *"unknown flag"* ]]
}

@test "non-numeric hours → exit 2" {
    run "$SCRIPT" abc
    [ "$status" -eq 2 ]
    [[ "$output" == *"hours must be a non-negative integer"* ]]
}

@test "TARGET_GLOB scope: real /tmp/ft-*-target is NEVER touched by tests" {
    # Sentinel: this test re-asserts the harness invariant.
    # If our TARGET_GLOB ever leaked, we'd risk eating a live agent's cache.
    [[ "$TARGET_GLOB" == "${TEST_DIR}/ft-*-target" ]]
    [[ "$TEST_DIR" == /tmp/clean-stale-test* ]]
    [[ "$TEST_DIR" != "/tmp" ]]
}

@test "operator lock: stale PID lock is recovered before cleanup" {
    make_target "ft-stale-target" 1500 >/dev/null
    mkdir "$FT_OPERATOR_LOCK_DIR"
    echo 999999 > "$FT_OPERATOR_LOCK_DIR/pid"
    echo "dead-holder" > "$FT_OPERATOR_LOCK_DIR/name"

    run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
    [ ! -d "$FT_OPERATOR_LOCK_DIR" ]
}

# ─── ft-v5lz3.2.8: active-usage skip path ────────────────────────────

@test "active-usage: dir flagged active is SKIPPED, not removed" {
    d="$(make_target "ft-stale-active-target" 1500)"
    make_target "ft-stale-idle-target" 1500 >/dev/null

    FT_TEST_FAKE_ACTIVE_DIRS="$d" run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"skipped $d (active usage)"* ]]
    [[ "$output" == *"removed "*"ft-stale-idle-target"* ]]
    [[ "$output" == *"cleaned 1 dirs (skipped 1)"* ]]
    # Active dir survived; idle dir is gone.
    [ -d "$d" ]
    [ ! -d "${TEST_DIR}/ft-stale-idle-target" ]
}

@test "active-usage: --dry-run reports skipped active dirs without touching idle ones" {
    d_active="$(make_target "ft-stale-active-target" 1500)"
    make_target "ft-stale-idle-target" 1500 >/dev/null

    FT_TEST_FAKE_ACTIVE_DIRS="$d_active" run "$SCRIPT" --dry-run 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"[dry-run] skipped $d_active (active usage)"* ]]
    [[ "$output" == *"[dry-run] would-remove"*"ft-stale-idle-target"* ]]
    [[ "$output" == *"cleaned 0 dirs (would have cleaned 1, skipped 1)"* ]]
    # Both dirs survive — dry run must never delete.
    [ "$(count_remaining)" -eq 2 ]
}

@test "active-usage: multiple active dirs all skipped" {
    d1="$(make_target "ft-stale-a-target" 1500)"
    d2="$(make_target "ft-stale-b-target" 1500)"
    d3="$(make_target "ft-stale-idle-target" 1500)"

    FT_TEST_FAKE_ACTIVE_DIRS="${d1}:${d2}" run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" == *"skipped $d1 (active usage)"* ]]
    [[ "$output" == *"skipped $d2 (active usage)"* ]]
    [[ "$output" == *"removed "*"ft-stale-idle-target"* ]]
    [[ "$output" == *"cleaned 1 dirs (skipped 2)"* ]]
    [ -d "$d1" ]
    [ -d "$d2" ]
    [ ! -d "$d3" ]
}

@test "active-usage: fresh dir below threshold is NOT subjected to the lsof check" {
    # The active-usage check must only run for dirs that already cross
    # the age threshold; otherwise we'd waste time lsof-walking every
    # candidate the script ignores. Verify the per-line skip log is
    # absent (the summary line always says "skipped 0", which is fine).
    d="$(make_target "ft-fresh-target" 30)"

    FT_TEST_FAKE_ACTIVE_DIRS="$d" run "$SCRIPT" 12
    [ "$status" -eq 0 ]
    [[ "$output" != *"skipped $d (active usage)"* ]]
    [[ "$output" == *"cleaned 0 dirs (skipped 0)"* ]]
    [ -d "$d" ]
}
