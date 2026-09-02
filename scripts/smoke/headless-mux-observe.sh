#!/usr/bin/env bash
# Headless observe smoke: the real vendored `frankenterm-mux-server` plus `ft`
# from the SAME build, no GUI. Proves discover -> attach -> list -> send ->
# observe -> detect on one machine and writes a JSON receipt plus a log.
#
# This is a dev signal, not a release gate by itself: it runs whatever binaries
# you point it at. It exists because the first time it was run (2026-09-02) it
# found that every client connect aborted the mux server (fixed in 647d87fd6),
# so run it after touching mux-server-impl/local.rs, promise/spawn.rs, the
# vendored streaming client, or the pattern engine.
#
# Usage:
#   scripts/smoke/headless-mux-observe.sh [BIN_DIR] [OUT_DIR]
#   BIN_DIR defaults to target/debug; needs `ft` and `frankenterm-mux-server`.
#   OUT_DIR defaults to a fresh mktemp dir; receives receipt.json and smoke.log.
#
# Receipt schema (ft.smoke.headless-mux-observe.v1): generated_at, host, commit,
# cli_version, mux_version, bin_dir, status (pass|fail), steps[] of
# {name, status (pass|fail), detail}. A receipt is `pass` only when every step
# passed; there is no skipped state.
#
# Known limits of the dev mux-server (recorded, not worked around):
#   - Before ft-xxfwy.35 lands, `--config-file` is silently ignored and the
#     server always binds RUNTIME_DIR/sock (~/.local/share/frankenterm/sock on
#     macOS). The script therefore refuses to run if that socket is live.
#   - Sends use --no-paste: the default bracketed paste is not executed by zsh.
set -u

BIN_DIR="${1:-target/debug}"
FT="$BIN_DIR/ft"
MUX="$BIN_DIR/frankenterm-mux-server"
for bin in "$FT" "$MUX"; do
  [ -x "$bin" ] || { echo "missing binary: $bin" >&2; exit 2; }
done

SOCK="${SOCK:-$HOME/.local/share/frankenterm/sock}"
if [ -S "$SOCK" ] && lsof -U 2>/dev/null | grep -q -- "$SOCK"; then
  echo "refusing to run: $SOCK is already served by a live process" >&2
  exit 2
fi

D="${2:-$(mktemp -d "${TMPDIR:-/tmp}/ft-smoke-XXXXXX")}"
mkdir -p "$D/.ft"
chmod 700 "$D/.ft"
printf '[storage]\ndb_path = "ft.db"\n' > "$D/ft.toml"
chmod 600 "$D/ft.toml"
LOG="$D/smoke.log"
RECEIPT="$D/receipt.json"
STEPS="$D/steps.jsonl"
: > "$STEPS"
echo "smoke dir: $D"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$LOG"; }
plain() { printf '%s' "$1" | sed -E $'s/\x1b\\[[0-9;]*m//g' | tr -s '\n' ' '; }
step() { # name status detail
  local detail
  detail=$(plain "$3")
  jq -cn --arg n "$1" --arg s "$2" --arg d "$detail" '{name:$n,status:$s,detail:$d}' >> "$STEPS"
  log "step $1: $2 ($detail)"
}
stop_children() {
  # Never `kill 0`: an unset pid would signal the whole process group (the
  # script included) before the receipt is written.
  [ -n "${WATCH:-}" ] && kill "$WATCH" 2>/dev/null
  [ -n "${MUX_PID:-}" ] && kill "$MUX_PID" 2>/dev/null
  return 0
}
finish() { # status
  local status="$1"
  jq -n \
    --arg schema "ft.smoke.headless-mux-observe.v1" \
    --arg generated_at "$(date -u +%FT%TZ)" \
    --arg host "$(hostname)" \
    --arg commit "$(git -C "$(dirname "$0")/../.." rev-parse --short HEAD 2>/dev/null || echo unknown)" \
    --arg cli_version "$("$FT" --version 2>/dev/null | head -1)" \
    --arg mux_version "$("$MUX" --version 2>/dev/null | head -1)" \
    --arg bin_dir "$BIN_DIR" \
    --arg status "$status" \
    --slurpfile steps "$STEPS" \
    '{schema:$schema,generated_at:$generated_at,host:$host,commit:$commit,cli_version:$cli_version,mux_version:$mux_version,bin_dir:$bin_dir,status:$status,steps:$steps}' \
    > "$RECEIPT"
  log "receipt: $RECEIPT (status=$status)"
}
fail() { # step-name detail
  step "$1" fail "$2"
  stop_children
  finish fail
  exit 1
}

# Bare zsh: nothing rewrites the pane title after we set it.
"$MUX" --daemonize=false --cwd "$D" -- /bin/zsh -f > "$D/mux.log" 2>&1 &
MUX_PID=$!
# A stale socket file from an earlier server satisfies `-S`; wait for the lease
# file to name THIS server's pid so a client never dials a dead socket.
for _ in $(seq 1 150); do grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null && [ -S "$SOCK" ] && break; sleep 0.2; done
grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null || fail mux_start "server pid $MUX_PID never took the socket lease: $(tail -3 "$D/mux.log" | tr '\n' ' ')"
step mux_start pass "pid $MUX_PID on $SOCK"

export WEZTERM_UNIX_SOCKET="$SOCK" FT_WORKSPACE="$D"
ft() { "$FT" -c "$D/ft.toml" "$@"; }

ft doctor --json > "$D/doctor.json" 2> "$D/doctor.err"
SOCK_ROW=$(jq -c '.checks[] | select(.name=="mux socket")' "$D/doctor.json" 2>/dev/null)
CONN_ROW=$(jq -c '.checks[] | select(.name=="WezTerm connection")' "$D/doctor.json" 2>/dev/null)
log "$SOCK_ROW"; log "$CONN_ROW"
jq -e '.checks[] | select(.name=="WezTerm connection") | .status == "ok"' "$D/doctor.json" > /dev/null \
  || fail doctor "did not reach the mux: $(tail -3 "$D/doctor.err" | tr '\n' ' ')"
step doctor pass "$(jq -r '.checks[] | select(.name=="WezTerm connection") | .detail' "$D/doctor.json")"

ft list --json > "$D/list.json" 2> "$D/list.err"
PANE=$(jq -r '.[0].pane_id' "$D/list.json" 2>/dev/null)
[ -n "$PANE" ] && [ "$PANE" != "null" ] || fail list "no pane listed: $(tail -3 "$D/list.err" | tr '\n' ' ')"
step list pass "pane $PANE"

ft watch --foreground --poll-interval 1000 > "$D/watch.log" 2>&1 &
WATCH=$!
sleep 5
grep -a -q 'Started vendored pane streaming subscription' "$D/watch.log" \
  || fail watch "no streaming subscription within 5 s: $(tail -3 "$D/watch.log" | tr '\n' ' ')"
step watch pass "streaming subscription for pane $PANE"

ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$D/send1.log" 2>&1 || fail send_title "$(tail -2 "$D/send1.log" | tr '\n' ' ')"
sleep 3
ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$D/send2.log" 2>&1 || fail send_limit "$(tail -2 "$D/send2.log" | tr '\n' ' ')"
step send pass "title set to codex; usage-limit line sent"
sleep 10

ft events -f json -l 5 > "$D/events.json" 2> "$D/events.err"
jq -c '.[] | {id, rule_id, agent_type, severity, extracted, matched_text}' "$D/events.json" 2>/dev/null | tee -a "$LOG"
jq -e 'any(.[]; .rule_id == "codex.usage.reached")' "$D/events.json" > /dev/null \
  || fail detect "no codex.usage.reached event ($(grep -c . "$D/watch.log") watch log lines in $D)"
step detect pass "$(jq -c '[.[] | select(.rule_id=="codex.usage.reached")][0] | {id, extracted}' "$D/events.json")"

DROPPED=$(grep -a -c 'Failed to persist segment' "$D/watch.log")
RESYNCS=$(grep -a -c 'Sequence discontinuity' "$D/watch.log")
if [ "$DROPPED" != "0" ] || [ "$RESYNCS" != "0" ]; then
  fail durability "dropped segments: $DROPPED, sequence resyncs: $RESYNCS (ft-xxfwy.32)"
fi
step durability pass "dropped segments 0, sequence resyncs 0"

stop_children
finish pass
echo "PASS: observe->detect on a real headless mux (evidence in $D)"
