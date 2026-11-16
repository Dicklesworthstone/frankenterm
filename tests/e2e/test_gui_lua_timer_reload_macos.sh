#!/usr/bin/env bash
# tests/e2e/test_gui_lua_timer_reload_macos.sh
#
# Native proof that a Lua time.call_after chain survives config reloads
# (ft-6vfeq.3). Operator profiles keep remote domains alive with a
# self-rescheduling watchdog started in gui-startup, which does not refire on
# reload; before the fix a reload could silently drop the pending callback and
# end the chain.
#
# The bundled GUI runs in the background (`open -g`) under a throwaway HOME
# with fixtures/gui_lua_timer_reload.lua. The fixture ticks once a second into
# $HOME/ticks. The driver measures the rate, forces five reloads by editing
# the config file (automatically_reload_config), checks that the GUI really
# re-evaluated it ($HOME/evals), and measures the rate again: it must stay one
# tick a second (not stop, not multiply).
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable.
#
# Env knobs:
#   FT_GUI_APP_PATH  FrankenTerm.app to test (default /Applications/FrankenTerm.app)
#   FT_TR_X, FT_TR_Y test window position (points, main screen)
#   FT_TR_KEEP       1 = keep the scratch directory
set -uo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'timer-reload: skipping (Darwin-only)\n' >&2
    exit 2
fi
APP="${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}"
[[ -x "$APP/Contents/MacOS/frankenterm-gui" ]] || {
    printf 'timer-reload: %s is not a FrankenTerm bundle\n' "$APP" >&2
    exit 2
}
HERE="$(cd "$(dirname "$0")" && pwd)"
E="$(mktemp -d /tmp/fttr.XXXXXX)"
mkdir -p "$E/home"
cp "$HERE/fixtures/gui_lua_timer_reload.lua" "$E/gui.lua"

log() { printf '[timer-reload %s] %s\n' "$(date +%T)" "$*" >&2; }
gui_pid() { pgrep -f -- "--config-file $E/gui.lua" | head -1; }
cleanup() {
    local pid
    pid="$(gui_pid)"
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    for _ in $(seq 20); do [[ -z "$(gui_pid)" ]] && break; sleep 0.25; done
    pid="$(gui_pid)"
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    if [[ "${FT_TR_KEEP:-0}" == 1 ]]; then log "kept $E"; else rm -rf -- "$E"; fi
}
trap cleanup EXIT

lines() { [[ -f "$1" ]] && wc -l < "$1" | tr -d ' ' || echo 0; }
# Ticks written over the next N seconds.
rate() {
    local before after
    before="$(lines "$E/home/ticks")"
    sleep "$1"
    after="$(lines "$E/home/ticks")"
    echo $(( after - before ))
}

open -g -n --env HOME="$E/home" --env FT_TR_HOME="$E/home" \
    --env FT_TR_X="${FT_TR_X:-40}" --env FT_TR_Y="${FT_TR_Y:-40}" \
    --env FRANKENTERM_LUA_CONFIG=1 --stderr "$E/home/gui.err" \
    -a "$APP" --args --config-file "$E/gui.lua" start
for _ in $(seq 60); do (( $(lines "$E/home/ticks") >= 3 )) && break; sleep 0.5; done
if (( $(lines "$E/home/ticks") < 3 )); then
    log "the chain never started (ticks: $(lines "$E/home/ticks"))"
    tail -5 "$E/home/gui.err" >&2 2>/dev/null
    exit 2
fi

FAILED=0
before_rate="$(rate 10)"
log "before reloads: $before_rate ticks in 10 s"
evals_before="$(lines "$E/home/evals")"
for n in 1 2 3 4 5; do
    printf -- '-- reload %d\n' "$n" >> "$E/gui.lua"
    sleep 2
done
sleep 2
evals_after="$(lines "$E/home/evals")"
reloads=$(( evals_after - evals_before ))
log "config evaluations during the reloads: $reloads"
if (( reloads < 1 )); then
    log "the GUI never reloaded its config; cannot judge"
    exit 2
fi
after_rate="$(rate 10)"
log "after $reloads reloads: $after_rate ticks in 10 s"
for check in "before:$before_rate" "after:$after_rate"; do
    value="${check#*:}"
    if (( value >= 8 && value <= 12 )); then
        log "ok   ${check%%:*} reloads: $value ticks in 10 s (want 8..12)"
    else
        log "FAIL ${check%%:*} reloads: $value ticks in 10 s (want 8..12; 0 = chain lost, 20+ = multiplied)"
        FAILED=1
    fi
done
if (( FAILED )); then
    log "FAIL"
    exit 1
fi
log "PASS"
exit 0
