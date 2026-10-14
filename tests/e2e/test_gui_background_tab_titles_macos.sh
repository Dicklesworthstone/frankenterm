#!/usr/bin/env bash
# tests/e2e/test_gui_background_tab_titles_macos.sh
#
# Native proof that a remote tab keeps its title up to date in the GUI even if
# the GUI has never shown it (ft-pzu8b).
#
# Why this exists: a mux server pushes a pane's title and output only to a
# client that has already asked about that pane, and the GUI used to ask only
# when it painted the pane. A tab attached behind another tab, created and left
# before its first frame, or reattached after a reconnect kept the title it had
# at that moment until the user clicked it.
#
# Each case runs a throwaway current-codec mux and the bundled GUI in the
# background (`open -g`) under a throwaway HOME. The mux holds one idle tab and
# two tabs whose program sets the title to tick-N once a second; the GUI stays
# on the idle tab and its Lua records every tab's title twice a second:
#   attach     a second GUI attaches to tabs a first GUI created
#   spawn      one GUI creates the tabs and goes straight back to the idle tab
#   reconnect  like attach, then the GUI's proxy is killed; the titles must
#              advance again after the reconnect
# A case passes when both background tabs show at least 5 distinct titles in
# the measurement window.
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable.
#
# Env knobs:
#   FT_GUI_APP_PATH  FrankenTerm.app to test (default /Applications/FrankenTerm.app)
#   FT_BG_CASES      cases to run (default "attach spawn reconnect")
#   FT_BG_SECONDS    measurement window per case (default 12)
#   FT_BG_PROXY      proxy command for an existing mux instead of the throwaway
#                    one (a codec 46 mux is expected to FAIL: it has the same
#                    push rule and is deliberately not asked about every pane)
#   FT_BG_KEEP       1 = keep the scratch directories
set -uo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'background-titles: skipping (Darwin-only)\n' >&2
    exit 2
fi
APP="${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}"
[[ -x "$APP/Contents/MacOS/frankenterm-gui" && -x "$APP/Contents/MacOS/frankenterm-mux-server" ]] || {
    printf 'background-titles: %s is not a FrankenTerm bundle\n' "$APP" >&2
    exit 2
}
HERE="$(cd "$(dirname "$0")" && pwd)"
SECONDS_PER_CASE="${FT_BG_SECONDS:-12}"
E=""
MUX_PID=""

log() { printf '[background-titles %s] %s\n' "$(date +%T)" "$*" >&2; }
gui_pid() { # home
    local pid
    for pid in $(pgrep -f "Contents/MacOS/frankenterm-gui"); do
        ps -E -p "$pid" -o command= 2>/dev/null | grep -q "HOME=$1" && { echo "$pid"; return; }
    done
}
stop_gui() { # home
    local pid
    pid="$(gui_pid "$1")"
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    for _ in $(seq 20); do [[ -z "$(gui_pid "$1")" ]] && return 0; sleep 0.25; done
    pid="$(gui_pid "$1")"
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
}
cleanup() {
    [[ -n "$E" ]] || return 0
    stop_gui "$E/g1"
    stop_gui "$E/g2"
    [[ -n "$MUX_PID" ]] && kill "$MUX_PID" 2>/dev/null
    MUX_PID=""
    sleep 1
    if [[ "${FT_BG_KEEP:-0}" == 1 ]]; then log "kept $E"; else rm -rf -- "$E"; fi
    E=""
}
trap cleanup EXIT

launch() { # home role
    open -g -n --env HOME="$1" --env FT_BG_HOME="$1" --env FT_BG_ROLE="$2" \
        --env FT_BG_SOCK="$E/mux/.local/share/frankenterm/sock" \
        ${FT_BG_PROXY:+--env "FT_BG_PROXY=$FT_BG_PROXY"} \
        --env FRANKENTERM_LUA_CONFIG=1 --stderr "$1/gui.err" \
        -a "$APP" --args --config-file "$E/gui.lua" connect e2e
}
wait_for_setup() { # home
    for _ in $(seq 80); do [[ -f "$1/setup.done" ]] && return 0; sleep 0.25; done
    log "the GUI did not create the background tabs"
    tail -3 "$1/gui.err" >&2 2>/dev/null
    return 1
}

FAILED=0
run_case() { # attach|spawn|reconnect
    local mode="$1" sock log_file columns column distinct ok=1 cut_at child
    E="$(mktemp -d /tmp/ftbg.XXXXXX)"
    mkdir -p "$E/mux" "$E/g1" "$E/g2"
    cp "$HERE/fixtures/gui_background_tab_titles.lua" "$E/gui.lua"
    sock="$E/mux/.local/share/frankenterm/sock"
    if [[ -z "${FT_BG_PROXY:-}" ]]; then
        HOME="$E/mux" "$APP/Contents/MacOS/frankenterm-mux-server" --skip-config --daemonize false \
            -- /bin/sh -c 'exec sleep 100000' >"$E/mux.log" 2>&1 </dev/null &
        MUX_PID=$!
        for _ in $(seq 40); do [[ -S "$sock" ]] && break; sleep 0.25; done
        [[ -S "$sock" ]] || { log "the throwaway mux did not start"; return 2; }
    fi
    if [[ "$mode" == spawn ]]; then
        launch "$E/g2" both
        wait_for_setup "$E/g2" || return 2
        sleep "$SECONDS_PER_CASE"
    else
        launch "$E/g1" setup
        wait_for_setup "$E/g1" || return 2
        sleep 1
        stop_gui "$E/g1"
        sleep 2
        launch "$E/g2" observe
        if [[ "$mode" == reconnect ]]; then
            sleep 6
            cut_at="$(date +%s)"
            # The GUI's proxy is its child; killing it drops the connection.
            for child in $(pgrep -P "$(gui_pid "$E/g2")"); do
                ps -o command= -p "$child" | grep -qE "nc -U|/bin/sh -c" && kill "$child"
            done
            # The reconnect has happened once the last tab's title moves on from
            # its value at the cut. Wait up to 30 s for that (a loaded host can
            # take well over the old fixed 3 s), then judge a full window from
            # there. Titles that never resume still fail.
            local frozen resumed_at="" resumed
            frozen="$(awk -v t="$cut_at" '$1 <= t' "$E/g2/titles.log" | tail -1 | awk -F'|' '{print $NF}')"
            for _ in $(seq 60); do
                resumed="$(awk -v t="$cut_at" '$1 > t' "$E/g2/titles.log" |
                    awk -F'|' -v f="$frozen" '$NF != f { print $1; exit }')"
                if [[ -n "$resumed" ]]; then
                    resumed_at="${resumed%% *}"
                    break
                fi
                sleep 0.5
            done
            [[ -n "$resumed_at" ]] || resumed_at="$cut_at"
            sleep "$SECONDS_PER_CASE"
            awk -v t="$resumed_at" '$1 >= t' "$E/g2/titles.log" >"$E/g2/titles.after"
            mv "$E/g2/titles.after" "$E/g2/titles.log"
        else
            sleep "$SECONDS_PER_CASE"
        fi
    fi
    log_file="$E/g2/titles.log"
    [[ -s "$log_file" ]] || { log "$mode: the GUI recorded no titles"; tail -3 "$E/g2/gui.err" >&2 2>/dev/null; return 2; }
    columns="$(tail -1 "$log_file" | cut -d' ' -f2- | awk -F'|' '{print NF}')"
    for column in $((columns - 1)) "$columns"; do
        distinct="$(cut -d' ' -f2- "$log_file" | cut -d'|' -f"$column" | sort -u | wc -l | tr -d ' ')"
        (( distinct >= 5 )) || ok=0
        log "     $mode: background tab $column showed $distinct distinct titles"
    done
    if (( ok )); then
        log "ok   $mode: $(tail -1 "$log_file" | cut -d' ' -f2-)"
    else
        log "FAIL $mode: titles froze at $(tail -1 "$log_file" | cut -d' ' -f2-)"
        FAILED=1
    fi
    return 0
}

for mode in ${FT_BG_CASES:-attach spawn reconnect}; do
    run_case "$mode"
    rc=$?
    cleanup
    (( rc == 2 )) && exit 2
done

if (( FAILED )); then
    log "FAIL"
    exit 1
fi
log "PASS"
exit 0
