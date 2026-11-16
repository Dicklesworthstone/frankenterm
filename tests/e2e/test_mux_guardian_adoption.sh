#!/usr/bin/env bash
# E2E: a mux adopts the guardian panes that no recovery root names (ft-upal7),
# through the host script (scripts/mux-guardian-host.sh).
#
#   test_mux_guardian_adoption.sh <family dir> [noroot|late] [host script]
#
# The family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build. Runs in a NEW isolated sandbox (own HOME, XDG dirs, sockets,
# FTG_INSTANCE) with the `direct` service manager, so it never touches a live
# mux, systemd or launchd. Nothing is deleted; every started process is stopped.
#
# E2E_PANES panes (default 10) each print E2E_ROWS numbered rows (default
# 5000), then sleep. The mux is killed with SIGKILL, the flood goes on for
# E2E_FLOOD_S seconds (default 20) with no mux, and the host script brings the
# mux back. Modes:
#   noroot (default): the mux-startup handler spawns every pane, then sleeps,
#     so the mux publishes no root (its first waits for startup). The host
#     script must start the next mux with --recovery-adopt. E2E_HOLD=0 leaves
#     out the sleep, so the kill lands while the first root is being captured;
#     a root published before the kill makes the run INVALID (exit 3).
#   late: the handler spawns half the panes, and the rest E2E_LATE_S seconds
#     (default 40) after the first root, with one root an hour, so no root
#     names them. The host script restores that root, and the restored mux
#     must adopt the rest.
# Then, once the mux that adopted them has published E2E_ROOTS roots of its own
# (default 3: a first root's capture keeps fewer rows of history than the next
# one's, and the one after that repeats the checkpoint before it), it is
# killed again and must come back from its newest (E2E_AGAIN=0 skips this).
#
# Pass criteria (all must hold): the mux adopts every pane no root names; every
# pane holds rows 1..E2E_ROWS exactly once after each restart; no mux log line
# names a quarantined lease or an indeterminate outcome.
# Exit 0 pass, 1 fail, 2 usage/setup error, 3 invalid run.
set -uo pipefail
# The guardian refuses group- or world-writable directories on its paths
# (a umask of 002 on some hosts); keep any stricter umask.
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
FAM=${1:?family dir}
MODE=${2:-noroot}
TOOL=${3:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 2; }
for b in frankenterm-mux-server frankenterm-pty-guardian ft; do
    [ -x "$FAM/$b" ] || { echo "FAIL: $FAM/$b not executable"; exit 2; }
done
ROWS=${E2E_ROWS:-5000}
PANES=${E2E_PANES:-10}
LATE_S=${E2E_LATE_S:-40}
FLOOD_S=${E2E_FLOOD_S:-20}
AGAIN=${E2E_AGAIN:-1}
HOLD=${E2E_HOLD:-1}
ROOTS=${E2E_ROOTS:-3}
case $MODE in
    noroot) INTERVAL=2; FIRST=$PANES ;;
    late) INTERVAL=3600; FIRST=$(( PANES / 2 )) ;;
    *) echo "FAIL: mode is noroot or late"; exit 2 ;;
esac
LATER=$(( PANES - FIRST ))

case $(uname -s) in Darwin) ROOT=/private/tmp ;; *) ROOT=/tmp ;; esac
# E2E_ROOT=DIR puts the sandbox elsewhere: a shared tmpfs /tmp that fills
# up mid-run fails stores for reasons outside the code under test.
ROOT=${E2E_ROOT:-$ROOT}
S=$ROOT/ftad-$$
mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; exit 2; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=e2e FTG_RECOVERY_INTERVAL=$INTERVAL
export FRANKENTERM_LUA_CONFIG=1 RUST_LOG=${E2E_RUST_LOG:-info}
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
cd "$HOME" || exit 2
STATE=$HOME/.local/state/frankenterm-guardian-e2e
RUN=$XDG_RUNTIME_DIR/frankenterm-guardian-e2e
[ "$(uname -s)" = Darwin ] && RUN=$STATE/run
SOCK=$RUN/mux.sock
ML=$STATE/logs/mux-server.log
export FTG_MUX_TOML=$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.lua

printf '#!/bin/sh\nsleep 4\n/usr/bin/awk -v n=%s '"'"'BEGIN{for(i=1;i<=n;i++){printf "%%09d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n", i}}'"'"'\nexec /bin/sleep 100000\n' \
    "$ROWS" > "$S/flood.sh"
FLOOD="{ args = { '/bin/sh', '$S/flood.sh' } }"
{
    echo "local wezterm = require 'wezterm'"
    echo "wezterm.on('mux-startup', function()"
    echo "  local _, _, window = wezterm.mux.spawn_window $FLOOD"
    echo "  for k = 2, $FIRST do window:spawn_tab $FLOOD end"
    if [ "$MODE" = noroot ]; then
        [ "$HOLD" = 0 ] || echo "  wezterm.sleep_ms(1800000)"
    else
        echo "  wezterm.time.call_after($LATE_S, function()"
        echo "    for k = 1, $LATER do window:spawn_tab $FLOOD end"
        echo "  end)"
    fi
    echo "end)"
    echo "return { scrollback_lines = $(( ROWS + 10000 )), unix_domains = { { name = 'unix', socket_path = [==[$SOCK]==] } } }"
} > "$FTG_MUX_TOML"

log() { printf '[adopt %s] %s\n' "$(date +%T)" "$*"; }
mux_log() { sed 's/\x1b\[[0-9;]*m//g' "$ML"; }
roots() { mux_log | grep -a -c 'recovery published local generation='; }
pane_ids() {
    WEZTERM_UNIX_SOCKET=$SOCK "$FAM/ft" robot state 2>/dev/null | python3 -c 'import json,sys
for p in json.load(sys.stdin).get("data") or []: print(p["pane_id"])' 2>/dev/null
}
wait_panes() { # <count> <seconds>
    for _ in $(seq "$2"); do
        [ "$(pane_ids | grep -c .)" -ge "$1" ] && return 0
        sleep 1
    done
    return 1
}
held() { # one line per pane: id, first, last, count of the numbered rows it holds
    local id
    for id in $(pane_ids); do
        WEZTERM_UNIX_SOCKET=$SOCK "$FAM/ft" robot get-text "$id" --tail 4000000 2>>"$S/get-text.err" \
            | grep -a -o '[0-9]\{9\} xxxxxxxx' | cut -c1-9 \
            | awk -v id="$id" 'NR==1{f=$1} {l=$1; c++} END{printf "%s %d %d %d\n", id, f+0, l+0, c+0}'
    done
}
verify() { # <label>: every pane up, each holding rows 1..ROWS once
    held > "$S/$1.txt"
    local up full
    up=$(grep -c . "$S/$1.txt")
    full=$(awk -v r="$ROWS" '$2 == 1 && $3 == r && $4 == r {c++} END{print c+0}' "$S/$1.txt")
    log "$1: $up panes up, $full of them holding every row 1..$ROWS"
    awk -v r="$ROWS" '!($2 == 1 && $3 == r && $4 == r) {print "  pane " $1 ": rows " $2 ".." $3 ", " $4 " held"}' "$S/$1.txt" | head -5
    [ "$up" = "$PANES" ] && [ "$full" = "$PANES" ]
}
quarantined() { # <label>: a lease quarantined, or an outcome left indeterminate, fails the run
    local n
    n=$(mux_log | grep -a -c 'quarantined\|outcome is indeterminate')
    [ "$n" = 0 ] && return 0
    log "$1: $n mux log lines name a quarantined lease or an indeterminate outcome"
    mux_log | grep -a 'quarantined\|outcome is indeterminate' | cut -c1-200 | head -3
    return 1
}
restart() { # <label>: kill the mux, then bring it back through the host script
    local mux start
    mux=$(cat "$STATE/run.mux-server.pid")
    kill -KILL "$mux" && log "mux $mux killed"
    for _ in $(seq 40); do kill -0 "$mux" 2>/dev/null || break; sleep 0.25; done
    if [ "$1" = first ]; then
        sleep "$FLOOD_S"
        log "the flood went on for $FLOOD_S s with no mux"
    fi
    start=$(date +%s)
    "$TOOL" install --family "$FAM" --manager direct > "$S/install-$1.log" 2>&1 \
        || { log "$1: the host script failed: $(tail -2 "$S/install-$1.log" | tr '\n' ' ')"; return 1; }
    log "$1: back in $(( $(date +%s) - start )) s: $(grep -a -o 'restoring [0-9]* guardian pane(s)\|adopting [0-9]* guardian pane(s)\|fresh mux start[^)]*)' "$S/install-$1.log" | tr '\n' ' ')"
    wait_panes "$PANES" 60
    sleep 5
}
cleanup() {
    "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true
    log "sandbox $S, $(du -sh "$S" 2>/dev/null | cut -f1)"
}
trap cleanup EXIT

log "family $FAM, mode $MODE: $PANES panes of $ROWS rows ($FIRST at startup)"
"$TOOL" install --family "$FAM" --manager direct > "$S/install-start.log" 2>&1 \
    || { log "install failed: $(tail -2 "$S/install-start.log" | tr '\n' ' ')"; exit 2; }
if [ "$MODE" = late ]; then
    for _ in $(seq 120); do [ "$(roots)" -ge 1 ] && break; sleep 1; done
    [ "$(roots)" -ge 1 ] || { log "no first root"; exit 2; }
    log "first root published with $(pane_ids | grep -c .) panes up"
fi
wait_panes "$PANES" $(( LATE_S + 120 )) || { log "only $(pane_ids | grep -c .) panes came up"; exit 2; }
log "$PANES panes up, roots published: $(roots)"
if [ "$MODE" = noroot ] && [ "$(roots)" != 0 ]; then
    log "INVALID: a root was published before the kill"
    exit 3
fi
[ "$MODE" = noroot ] || sleep "$FLOOD_S"
restart first || exit 1
ADOPTED=$(mux_log | grep -a -o 'adopted [0-9]* guardian panes' | tail -1 | awk '{print $2}')
EXPECT=$PANES; [ "$MODE" = late ] && EXPECT=$LATER
log "the mux adopted ${ADOPTED:-no} panes (expected $EXPECT)"
mux_log | grep -a 'stays unadopted\|cannot be registered\|has no window' | cut -c1-200 | sort | uniq -c | head -5
OK=1
verify adopted || OK=0
[ "${ADOPTED:-0}" = "$EXPECT" ] || { log "expected $EXPECT adopted panes"; OK=0; }
if [ "$AGAIN" = 1 ]; then
    before=$(roots)
    for _ in $(seq 900); do [ "$(roots)" -ge $(( before + ROOTS )) ] && break; sleep 1; done
    if [ "$(roots)" -ge $(( before + ROOTS )) ]; then
        log "the adopting mux published $ROOTS roots of its own"
        quarantined adopting || OK=0
        restart again || exit 1
        verify again || OK=0
    else
        log "the adopting mux published $(( $(roots) - before )) of $ROOTS roots in 900 s"
        quarantined adopting
        OK=0
    fi
fi
quarantined end || OK=0
if [ "$OK" = 1 ]; then
    log "PASS: every pane came back holding every row"
    exit 0
fi
log "FAIL"
exit 1
