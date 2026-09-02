#!/usr/bin/env bash
# Headless observe smoke: the real vendored `frankenterm-mux-server` plus `ft`
# from the SAME build, no GUI. Proves discover -> attach -> list -> send ->
# observe -> detect on one machine and prints the evidence.
#
# This is a dev signal, not a release gate: it runs whatever binaries you point
# it at. It exists because the first time it was run (2026-09-02) it found that
# every client connect aborted the mux server (fixed in 647d87fd6), so run it
# after touching mux-server-impl/local.rs, promise/spawn.rs, the vendored
# streaming client, or the pattern engine.
#
# Usage:
#   scripts/smoke/headless-mux-observe.sh [BIN_DIR]
#   BIN_DIR defaults to target/debug; needs `ft` and `frankenterm-mux-server`.
#
# Known limits of the dev mux-server (recorded, not worked around):
#   - `--config-file` is silently ignored (no logger is installed), so the
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

D=$(mktemp -d "${TMPDIR:-/tmp}/ft-smoke-XXXXXX")
mkdir -p "$D/.ft"
chmod 700 "$D/.ft"
printf '[storage]\ndb_path = "ft.db"\n' > "$D/ft.toml"
chmod 600 "$D/ft.toml"
echo "smoke dir: $D"

fail() { echo "FAIL: $*" >&2; kill "${WATCH:-0}" "${MUX_PID:-0}" 2>/dev/null; exit 1; }

# Bare zsh: nothing rewrites the pane title after we set it.
"$MUX" --daemonize=false --cwd "$D" -- /bin/zsh -f > "$D/mux.log" 2>&1 &
MUX_PID=$!
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.2; done
[ -S "$SOCK" ] || fail "mux socket never appeared ($(cat "$D/mux.log"))"
echo "mux-server pid $MUX_PID on $SOCK"

export WEZTERM_UNIX_SOCKET="$SOCK" FT_WORKSPACE="$D"
ft() { "$FT" -c "$D/ft.toml" "$@"; }

echo "=== doctor"
ft doctor --json > "$D/doctor.json" 2> "$D/doctor.err"
jq -c '.checks[] | select(.name=="mux socket" or .name=="WezTerm connection")' "$D/doctor.json"
jq -e '.checks[] | select(.name=="WezTerm connection") | .status == "ok"' "$D/doctor.json" > /dev/null \
  || fail "doctor did not reach the mux ($(cat "$D/doctor.err" | tail -3))"

echo "=== list"
ft list --json > "$D/list.json" 2> "$D/list.err"
PANE=$(jq -r '.[0].pane_id' "$D/list.json" 2>/dev/null)
[ -n "$PANE" ] && [ "$PANE" != "null" ] || fail "no pane listed ($(tail -3 "$D/list.err"))"
echo "pane id: $PANE"

echo "=== watch"
ft watch --foreground --poll-interval 1000 > "$D/watch.log" 2>&1 &
WATCH=$!
sleep 5

echo "=== send (title -> codex, then the codex usage-limit message)"
ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$D/send1.log" 2>&1 || fail "send 1 ($(tail -2 "$D/send1.log"))"
sleep 3
ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$D/send2.log" 2>&1 || fail "send 2"
sleep 10

echo "=== events"
ft events -f json -l 5 > "$D/events.json" 2> "$D/events.err"
jq -c '.[] | {id, rule_id, agent_type, severity, extracted, matched_text}' "$D/events.json" 2>/dev/null
jq -e 'any(.[]; .rule_id == "codex.usage.reached")' "$D/events.json" > /dev/null \
  || fail "no codex.usage.reached event (watch log: $(grep -c . "$D/watch.log") lines in $D/watch.log)"

echo "=== watcher warnings (informational)"
grep -a -c 'Failed to persist segment' "$D/watch.log" | sed 's/^/dropped segments: /'
grep -a -c 'Sequence discontinuity' "$D/watch.log" | sed 's/^/sequence resyncs: /'

kill "$WATCH" 2>/dev/null; sleep 1; kill "$MUX_PID" 2>/dev/null
echo "PASS: observe->detect on a real headless mux (evidence in $D)"
