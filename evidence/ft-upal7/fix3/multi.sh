#!/bin/bash
# A guardian-backed mux of N panes, each printing numbered rows at a chosen retention. Kills the
# mux, brings it back and compares what every pane holds, and how long the host script waited for
# it. Reads nothing while the flood runs.
#   multi.sh <FrankenTerm.app> <scrollback_lines> <rows a pane> <panes> [settle s]
# QUEUE_MB=<n> sets scrollback_durability_queue_max_mb (default 64), to force overload gaps.
# Exit 0 only if the mux came back and every pane holds its very last row.
set -uo pipefail
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
APP=$1; LINES_CFG=$2; ROWS=$3; PANES=$4; SETTLE=${5:-120}
FAM="$APP/Contents/MacOS"
TOOL=/Users/jemanuel/projects/frankenterm/scripts/mux-guardian-host.sh
S=/private/tmp/ftrt-$$
mkdir -m 700 "$S" || exit 2
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=e2e FTG_RECOVERY_INTERVAL=2 RUST_LOG=${E2E_RUST_LOG:-info}
export FRANKENTERM_LUA_CONFIG=1
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
STATE=$HOME/.local/state/frankenterm-guardian-e2e
SOCK=$STATE/run/mux.sock
ML=$STATE/logs/mux-server.log
export FTG_MUX_TOML=$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.lua

printf '#!/bin/sh\nsleep 4\n/usr/bin/awk -v n=%s '"'"'BEGIN{for(i=1;i<=n;i++){printf "%%09d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n", i}}'"'"'\nexec /bin/sleep 100000\n' \
    "$ROWS" > "$S/flood.sh"
{
    echo "local wezterm = require 'wezterm'"
    echo "wezterm.on('mux-startup', function()"
    echo "  local _, _, window = wezterm.mux.spawn_window { args = { '/bin/sh', '$S/flood.sh' } }"
    echo "  for k = 2, $PANES do window:spawn_tab { args = { '/bin/sh', '$S/flood.sh' } } end"
    echo "end)"
    echo "return { scrollback_lines = $LINES_CFG,${QUEUE_MB:+ scrollback_durability_queue_max_mb = $QUEUE_MB,} unix_domains = { { name = 'unix', socket_path = [==[$SOCK]==] } } }"
} > "$FTG_MUX_TOML"

log() { printf '[multi %s] %s\n' "$(date +%T)" "$*"; }
pane_ids() {
    WEZTERM_UNIX_SOCKET=$SOCK "$FAM/ft" robot state 2>/dev/null | /usr/bin/python3 -c 'import json,sys
for p in json.load(sys.stdin).get("data") or []: print(p["pane_id"])' 2>/dev/null
}
show() {
    sed 's/\x1b\[[0-9;]*m//g' "$ML" \
        | grep -a 'exceeds its limit\|validation failed\|terminating\|cannot publish\|allocation envelope\|predates\|deadline\|store continues\|checkpoint.*fail\|capture.*fail\|recovery published\|verified the recovery root\|guardian panes in\|backing off\|disabled=' \
        | sed 's/generation=[0-9]* in [0-9]* ms.*/generation=N/; s/here by [0-9]* rows/here by N rows/; s/pane [0-9]* store continues/pane N store continues/; s/: [0-9]* matched, [0-9]* to append/: N matched, M to append/' \
        | cut -c1-180 | sed 's/^\[[^ ]* //' | sort | uniq -c | sort -rn | head -10
}
held() { # one line per pane: id, first, last, count of the numbered rows it holds
    local id
    for id in $(pane_ids); do
        WEZTERM_UNIX_SOCKET=$SOCK "$FAM/ft" robot get-text "$id" --tail 4000000 2>>"$S/get-text.err" \
            | grep -a -o '[0-9]\{9\} xxxxxxxx' | cut -c1-9 \
            | awk -v id="$id" 'NR==1{f=$1} {l=$1; c++} END{printf "%s %d %d %d\n", id, f+0, l+0, c+0}'
    done
}
cleanup() {
    : > "$S/stall.end"
    [ -n "${GUARDIAN:-}" ] && kill -CONT "$GUARDIAN" 2>/dev/null
    "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true
    log "sandbox $S, $(/usr/bin/du -sh "$S" 2>/dev/null | cut -f1)"
}
trap cleanup EXIT

# This machine is shared. A mux that holds several cores for a minute, once its panes' flood has
# had time to end, is a failure of this run: stop it rather than let it take the machine down.
CUTOFF_PCT=${CUTOFF_PCT:-800}
CUTOFF_AFTER=${CUTOFF_AFTER:-120}
watchdog() {
    local main=$1 over=0 pid pct started=$(date +%s)
    while kill -0 "$main" 2>/dev/null; do
        sleep 10
        [ $(( $(date +%s) - started )) -ge "$CUTOFF_AFTER" ] || continue
        pid=$(cat "$STATE/run.mux-server.pid" 2>/dev/null) || continue
        pct=$(ps -o pcpu= -p "$pid" 2>/dev/null | cut -d. -f1 | tr -d ' ')
        if [ "${pct:-0}" -ge "$CUTOFF_PCT" ]; then over=$(( over + 1 )); else over=0; fi
        if [ "$over" -ge 6 ]; then
            log "CUTOFF: mux $pid at ${pct}% CPU for a minute; stopping this run"
            kill -TERM "$main"
            return
        fi
    done
}
watchdog $$ &
log "app $APP retention $LINES_CFG, $PANES panes of $ROWS rows"
"$TOOL" install --family "$FAM" --manager direct >/dev/null || { log "install failed"; show; exit 2; }
first_root() { # seconds from the first ready line to the first published root, and the capture's time
    sed 's/\x1b\[[0-9;]*m//g' "$ML" | /usr/bin/python3 -c 'import re,sys,datetime
ready=root=None
for line in sys.stdin:
    m=re.match(r"\[(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ)", line)
    if not m: continue
    t=datetime.datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%SZ")
    if ready is None and "frankenterm-mux-server-ready" in line: ready=t
    if root is None and "recovery published local generation=" in line:
        root=t; took=re.search(r"in (\d+) ms", line)
if ready and root: print("first root %d s after ready (capture %s ms)" % ((root-ready).total_seconds(), took.group(1) if took else "?"))
else: print("no root yet")'
}
GUARDIAN=$(cat "$STATE/run.pty-guardian.pid" 2>/dev/null)
log "guardian pid ${GUARDIAN:-unknown}"
if [ -n "${SAMPLE_GUARDIAN:-}" ] && [ -n "$GUARDIAN" ]; then
    # SAMPLE_GUARDIAN=<delay s>/<seconds>[,<delay s>/<seconds>...] with SAMPLE_OUT=<path prefix>
    # outside the sandbox: /usr/bin/sample of the guardian at each delay after the install.
    n=0
    for window in ${SAMPLE_GUARDIAN//,/ }; do
        n=$(( n + 1 ))
        ( sleep "${window%/*}"; /usr/bin/sample "$GUARDIAN" "${window#*/}" -file "${SAMPLE_OUT:?}-$n.txt" \
            > /dev/null 2>&1; log "guardian sample $n written" ) &
    done
fi
MUXPID=$(cat "$STATE/run.mux-server.pid" 2>/dev/null)
if [ -n "${SAMPLE_MUX:-}" ] && [ -n "$MUXPID" ]; then
    # SAMPLE_MUX=<delay s>/<seconds>[,...] with SAMPLE_OUT: the same for the first mux server,
    # to <prefix>-mux-N.txt (symbols need a release-perf mux server).
    log "mux pid $MUXPID"
    n=0
    for window in ${SAMPLE_MUX//,/ }; do
        n=$(( n + 1 ))
        ( sleep "${window%/*}"; /usr/bin/sample "$MUXPID" "${window#*/}" -file "${SAMPLE_OUT:?}-mux-$n.txt" \
            > /dev/null 2>&1; log "mux sample $n written" ) &
    done
fi
if [ -n "${THROTTLE_GUARDIAN:-}" ] && [ -n "$GUARDIAN" ]; then
    # A guardian at background priority (throttled CPU and I/O) until the first root or the
    # settle time: every checkpoint round trip slows, as on a busy or sleeping machine.
    taskpolicy -b -p "$GUARDIAN" && log "guardian $GUARDIAN at background priority"
fi
STALLER=
if [ -n "${STALL_GUARDIAN:-}" ] && [ -n "$GUARDIAN" ]; then
    # STALL_GUARDIAN=<run s>/<stop s>: the guardian alternately stopped and running until the
    # first root or the settle time, so every checkpoint round trip takes several times longer.
    RUN_S=${STALL_GUARDIAN%/*}; STOP_S=${STALL_GUARDIAN#*/}
    # First let the mux spawn all its panes at full speed (up to two minutes).
    for _ in $(seq 120); do
        [ "$(pane_ids | grep -c .)" -ge "$PANES" ] && break
        sleep 1
    done
    log "panes before the stall: $(pane_ids | grep -c .)"
    (
        while [ ! -e "$S/stall.end" ]; do
            kill -STOP "$GUARDIAN" 2>/dev/null || break
            sleep "$STOP_S"
            kill -CONT "$GUARDIAN" 2>/dev/null
            sleep "$RUN_S"
        done
        kill -CONT "$GUARDIAN" 2>/dev/null
    ) &
    STALLER=$!
    log "guardian $GUARDIAN stalled: $RUN_S s running, then $STOP_S s stopped, over and over"
fi
if [ -n "${KILL_AT_ROOT:-}" ]; then
    # Kill as soon as the first root is published, so the restore comes from that root.
    for _ in $(seq "$SETTLE"); do
        sed 's/\x1b\[[0-9;]*m//g' "$ML" | grep -a -q 'recovery published local generation=' && break
        sleep 1
    done
    log "$(first_root)"
    log "before the kill (no reads):"
    show
else
    sleep "$SETTLE"
    log "before the kill: $(first_root)"
    show
    held > "$S/before.txt"
    log "panes up: $(grep -c . "$S/before.txt"); rows held: $(awk '{printf "%s:%s ", $1, $4}' "$S/before.txt")"
    awk -v r="$ROWS" '$3 != r {print "WARNING: pane " $1 " last row " $3 " of " r}' "$S/before.txt"
fi

if [ -n "$STALLER" ]; then
    : > "$S/stall.end"
    wait "$STALLER"
    kill -CONT "$GUARDIAN" 2>/dev/null
    log "guardian $GUARDIAN running again"
fi
if [ -n "${THROTTLE_GUARDIAN:-}${STALLER}" ] && [ -n "$GUARDIAN" ]; then
    log "reuse lines: $(sed 's/\x1b\[[0-9;]*m//g' "$ML" | grep -a -c 'takes back the guardian checkpoint'); capture deadlines: $(sed 's/\x1b\[[0-9;]*m//g' "$ML" | grep -a -c 'deadline')"
fi
if [ -n "${THROTTLE_GUARDIAN:-}" ] && [ -n "$GUARDIAN" ]; then
    taskpolicy -B -p "$GUARDIAN" && log "guardian $GUARDIAN back at normal priority"
fi
MUX=$(cat "$STATE/run.mux-server.pid")
kill -KILL "$MUX" && log "mux $MUX killed"
for _ in $(seq 40); do kill -0 "$MUX" 2>/dev/null || break; sleep 0.25; done
START=$(date +%s)
if "$TOOL" install --family "$FAM" --manager direct >/dev/null; then
    TOOK=$(( $(date +%s) - START ))
    log "came back in $TOOK s"
else
    log "the host script gave up after $(( $(date +%s) - START )) s; watching the mux itself"
    TOOK=
    for _ in $(seq 300); do
        NOW=$(sed 's/\x1b\[[0-9;]*m//g' "$ML" | awk -v m="restoring $PANES" 'index($0, m){p=1; r=""} p && /frankenterm-mux-server-ready/{r="ready"} p && /deadline/{r=$0} END{print r}')
        [ -n "$NOW" ] && break
        sleep 1
    done
    log "the mux itself, $(( $(date +%s) - START )) s after the restart: ${NOW:-still restoring}"
    case $NOW in
        ready) TOOK=$(( $(date +%s) - START )) ;;
        *) sed 's/\x1b\[[0-9;]*m//g' "$ML" | tail -6 | cut -c1-220; exit 1 ;;
    esac
fi
sleep 5
log "after:"
show
held > "$S/after.txt"
log "panes up: $(grep -c . "$S/after.txt"); rows held: $(awk '{printf "%s:%s ", $1, $4}' "$S/after.txt")"
BAD=$(awk -v r="$ROWS" '$3 != r {c++} END{print c+0}' "$S/after.txt")
UP=$(grep -c . "$S/after.txt")
log "panes holding every row 1..$ROWS: $(awk -v r="$ROWS" '$2 == 1 && $3 == r && $4 == r {c++} END{print c+0}' "$S/after.txt") of $UP"
if [ "$BAD" = 0 ] && [ "$UP" = "$PANES" ]; then
    log "PASS: all $UP panes hold their newest row; back in $TOOK s (the host script waits up to $(( 60 + 5 * PANES )) s)"
    exit 0
fi
log "FAIL: $BAD of $UP panes lost their newest row"
exit 1
