#!/usr/bin/env bash
# Demo-full preflight verifier (br-ft-xl2kc.1).
#
# Runs the bead's pre-recording verification checklist against a
# live staged swarm. Operators run this before invoking
# `vhs scripts/demo-full.tape` so a failed precondition is caught
# *before* the recording starts, not after a 5-minute capture
# session reveals a missing rate-limit event.
#
# The script is a thin glue layer over `ft` subcommands the
# operator has already shipped. It does not stage the swarm
# itself (real cc/cod/gmi accounts are required and per-operator),
# but every other Acceptance item is mechanically checkable.
#
# Usage:
#   scripts/demo-full-preflight.sh [--json] [--since 5m]
#
# Output:
#   --json mode: a single JSON object on stdout describing each
#                check + overall pass/fail.
#   plain mode: human-readable check rows on stderr; final
#               PASS/FAIL summary on stdout.
#
# Exit codes:
#   0 — all preconditions met; safe to record.
#   1 — argument or wrapper error.
#   2 — one or more preconditions failed; fix + re-run.

set -uo pipefail

JSON_MODE=false
SINCE="5m"
MIN_PANES=10
MIN_RATE_LIMIT_EVENTS=1
MIN_WORKFLOW_RECOVERY=1
MIN_SEARCH_HITS=1
MIN_MISSIONS=1

usage() {
    cat <<EOF
demo-full preflight verifier (ft-xl2kc.1)

Usage: $0 [--json] [--since DURATION]

Options:
  --json              Emit JSON to stdout instead of human prose.
  --since DURATION    Time window for ft search / event lookups
                      (default: 5m).
  --min-panes N       Override the >=10 pane threshold.
  -h, --help          Show this help.

Acceptance items checked (per ft-xl2kc.1 Acceptance):
  1. >= ${MIN_PANES} panes alive (ft status --format toon).
  2. >= ${MIN_RATE_LIMIT_EVENTS} rate_limit event (ft robot events).
  3. >= ${MIN_WORKFLOW_RECOVERY} rate_limit_recovery workflow run.
  4. >= ${MIN_SEARCH_HITS} 'Connection refused' search hit.
  5. >= ${MIN_MISSIONS} robot mission with a TX log.

Cross-references:
  - scripts/demo-full.tape (recording input — invoke vhs against
    this after preflight passes).
  - docs/release/checklist.md (re-recording cadence).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --json)        JSON_MODE=true; shift ;;
        --since)       SINCE="$2"; shift 2 ;;
        --min-panes)   MIN_PANES="$2"; shift 2 ;;
        -h|--help)     usage; exit 0 ;;
        *)             echo "unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if ! command -v ft >/dev/null 2>&1; then
    echo "error: ft not on PATH; build with cargo build --release --bin ft and re-run" >&2
    exit 1
fi

# Per-check status: pass | fail | skipped
declare -a CHECK_NAMES=()
declare -a CHECK_STATUS=()
declare -a CHECK_DETAILS=()
declare -a CHECK_THRESHOLDS=()
declare -a CHECK_OBSERVED=()

record_check() {
    CHECK_NAMES+=("$1")
    CHECK_STATUS+=("$2")
    CHECK_DETAILS+=("$3")
    CHECK_THRESHOLDS+=("$4")
    CHECK_OBSERVED+=("$5")
}

# 1. Pane count
panes_output=$(ft status --format toon 2>/dev/null) || panes_output=""
if [ -n "$panes_output" ]; then
    pane_count=$(echo "$panes_output" | grep -cE '^[[:space:]]*[a-zA-Z_-]+' || echo 0)
    if [ "$pane_count" -ge "$MIN_PANES" ]; then
        record_check "panes_alive" "pass" "$pane_count panes" "$MIN_PANES" "$pane_count"
    else
        record_check "panes_alive" "fail" "$pane_count panes (need >= $MIN_PANES)" "$MIN_PANES" "$pane_count"
    fi
else
    record_check "panes_alive" "fail" "ft status returned no output" "$MIN_PANES" "0"
fi

# 2. Rate-limit events
rate_limit_count=$(ft robot events --filter rate_limit --json 2>/dev/null \
    | grep -oE '"event"' | wc -l | tr -d ' ' || echo 0)
if [ "$rate_limit_count" -ge "$MIN_RATE_LIMIT_EVENTS" ]; then
    record_check "rate_limit_events" "pass" "$rate_limit_count events" "$MIN_RATE_LIMIT_EVENTS" "$rate_limit_count"
else
    record_check "rate_limit_events" "fail" "$rate_limit_count events (need >= $MIN_RATE_LIMIT_EVENTS)" "$MIN_RATE_LIMIT_EVENTS" "$rate_limit_count"
fi

# 3. Workflow recovery
workflow_count=$(ft robot workflow recent --filter rate_limit_recovery --json 2>/dev/null \
    | grep -oE '"workflow"' | wc -l | tr -d ' ' || echo 0)
if [ "$workflow_count" -ge "$MIN_WORKFLOW_RECOVERY" ]; then
    record_check "rate_limit_workflow" "pass" "$workflow_count workflow runs" "$MIN_WORKFLOW_RECOVERY" "$workflow_count"
else
    record_check "rate_limit_workflow" "fail" "$workflow_count runs (need >= $MIN_WORKFLOW_RECOVERY)" "$MIN_WORKFLOW_RECOVERY" "$workflow_count"
fi

# 4. Search recovery
search_count=$(ft search 'Connection refused' --since "$SINCE" --format json 2>/dev/null \
    | grep -oE '"hit"' | wc -l | tr -d ' ' || echo 0)
if [ "$search_count" -ge "$MIN_SEARCH_HITS" ]; then
    record_check "search_recovery" "pass" "$search_count hits" "$MIN_SEARCH_HITS" "$search_count"
else
    record_check "search_recovery" "fail" "$search_count hits (need >= $MIN_SEARCH_HITS in last $SINCE; provoke a Connection refused if missing)" "$MIN_SEARCH_HITS" "$search_count"
fi

# 5. Robot missions with TX log
mission_count=$(ft robot mission list --json 2>/dev/null \
    | grep -oE '"tx_log"' | wc -l | tr -d ' ' || echo 0)
if [ "$mission_count" -ge "$MIN_MISSIONS" ]; then
    record_check "missions_with_tx" "pass" "$mission_count missions with TX log" "$MIN_MISSIONS" "$mission_count"
else
    record_check "missions_with_tx" "fail" "$mission_count (need >= $MIN_MISSIONS)" "$MIN_MISSIONS" "$mission_count"
fi

# Aggregate
overall=pass
fail_count=0
for status in "${CHECK_STATUS[@]}"; do
    if [ "$status" != "pass" ]; then
        overall=fail
        fail_count=$((fail_count + 1))
    fi
done

if $JSON_MODE; then
    printf '{\n'
    printf '  "bead": "ft-xl2kc.1",\n'
    printf '  "schema_version": 1,\n'
    printf '  "since": "%s",\n' "$SINCE"
    printf '  "overall_status": "%s",\n' "$overall"
    printf '  "fail_count": %d,\n' "$fail_count"
    printf '  "checks": [\n'
    last_idx=$((${#CHECK_NAMES[@]} - 1))
    for i in "${!CHECK_NAMES[@]}"; do
        sep=","
        if [ "$i" -eq "$last_idx" ]; then sep=""; fi
        printf '    {"name":"%s","status":"%s","threshold":%s,"observed":%s,"detail":"%s"}%s\n' \
            "${CHECK_NAMES[$i]}" \
            "${CHECK_STATUS[$i]}" \
            "${CHECK_THRESHOLDS[$i]}" \
            "${CHECK_OBSERVED[$i]}" \
            "${CHECK_DETAILS[$i]//\"/\\\"}" \
            "$sep"
    done
    printf '  ]\n'
    printf '}\n'
else
    {
        for i in "${!CHECK_NAMES[@]}"; do
            symbol=$([ "${CHECK_STATUS[$i]}" = "pass" ] && echo "✓" || echo "✗")
            printf "%s %-24s %s\n" "$symbol" "${CHECK_NAMES[$i]}" "${CHECK_DETAILS[$i]}"
        done
    } >&2
    if [ "$overall" = "pass" ]; then
        echo "PASS: all $fail_count preconditions met; safe to run vhs scripts/demo-full.tape"
    else
        echo "FAIL: $fail_count of ${#CHECK_NAMES[@]} preconditions unmet"
    fi
fi

if [ "$overall" = "pass" ]; then
    exit 0
else
    exit 2
fi
