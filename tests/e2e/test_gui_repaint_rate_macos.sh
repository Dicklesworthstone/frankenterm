#!/usr/bin/env bash
# tests/e2e/test_gui_repaint_rate_macos.sh
#
# Native proof that the GUI repaints a window only for what it shows, and that
# a full-window gradient background does not force a huge glyph atlas.
#
# Why this exists: a GUI with 11 stacked windows of agent panes repainted every
# window at max_fps around the clock (about 340 fps, over a core), because any
# pane's output or title change repainted every window, and each window's
# gradient background forced its glyph atlas to 8192^2 (256 MiB).
#
# Each case launches the bundled GUI in the background (`open -g`) under a
# throwaway HOME and reads the GUI's own counters from
# frankenterm-resources-<pid>.json (frames.presented_total, atlas live_bytes):
#   visible         visible tab prints 250 lines/s        -> paints at max_fps
#   hidden_output   only a background tab prints          -> window stays quiet
#   hidden_title    only a background tab's title changes -> well under max_fps
#   unfocused_cap   visible output, unfocused_max_fps=10  -> 10 fps
#   gradient        maximized window, gradient background -> small atlas
#
# The small test window must be drawn by macOS, so it has to sit where no
# opaque window covers it. By default that is the strip at the bottom of the
# main screen; override with FT_RR_X / FT_RR_Y (points, main screen).
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable (including a test
# window that macOS is not drawing).
#
# Env knobs:
#   FT_GUI_APP_PATH  FrankenTerm.app to test (default /Applications/FrankenTerm.app)
#   FT_RR_X, FT_RR_Y test window position
#   FT_RR_SECONDS    measurement window per case (default 12)
#   FT_RR_KEEP       1 = keep the scratch directory
set -uo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'repaint-rate: skipping (Darwin-only)\n' >&2
    exit 2
fi
APP="${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}"
[[ -x "$APP/Contents/MacOS/frankenterm-gui" ]] || {
    printf 'repaint-rate: %s is not a FrankenTerm bundle\n' "$APP" >&2
    exit 2
}
HERE="$(cd "$(dirname "$0")" && pwd)"
E="$(mktemp -d /tmp/ftrr.XXXXXX)"
cp "$HERE/fixtures/gui_repaint_rate.lua" "$E/gui.lua"
SECONDS_PER_CASE="${FT_RR_SECONDS:-12}"

log() { printf '[repaint-rate %s] %s\n' "$(date +%T)" "$*" >&2; }
gui_pid() { pgrep -f -- "--config-file $E/gui.lua" | head -1; }
stop_gui() {
    local pid
    pid="$(gui_pid)"
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
    for _ in $(seq 20); do [[ -z "$(gui_pid)" ]] && return 0; sleep 0.25; done
    gui_pid | xargs kill -9 2>/dev/null
}
cleanup() {
    stop_gui
    if [[ "${FT_RR_KEEP:-0}" == 1 ]]; then log "kept $E"; else rm -rf -- "$E"; fi
}
trap cleanup EXIT

# Default position: the bottom strip of the main screen.
if [[ -z "${FT_RR_X:-}" || -z "${FT_RR_Y:-}" ]]; then
    read -r SW SH < <(system_profiler SPDisplaysDataType 2>/dev/null |
        sed -n 's/.*UI Looks like: \([0-9]*\) x \([0-9]*\).*/\1 \2/p' | head -1)
    FT_RR_X="${FT_RR_X:-$(( ${SW:-1440} - 480 ))}"
    FT_RR_Y="${FT_RR_Y:-$(( ${SH:-900} - 78 ))}"
fi

counter() { # json-key-path
    plutil -extract "$1" raw -o - "$E/home/.local/share/frankenterm/frankenterm-resources-$(gui_pid).json" 2>/dev/null
}
launch() { # mode [unfocused_max_fps]
    rm -rf -- "${E:?}/home"
    mkdir -p "$E/home"
    open -g -n --env HOME="$E/home" --env FT_RR_HOME="$E/home" --env FT_RR_MODE="$1" \
        --env FT_RR_X="$FT_RR_X" --env FT_RR_Y="$FT_RR_Y" ${2:+--env "FT_RR_UNFOCUSED=$2"} \
        --env FRANKENTERM_LUA_CONFIG=1 --stderr "$E/home/gui.err" \
        -a "$APP" --args --config-file "$E/gui.lua" start
    for _ in $(seq 40); do [[ -n "$(counter frames.presented_total)" ]] && return 0; sleep 0.25; done
    log "the GUI published no counters for mode $1"
    return 1
}
fps() { # frames per second over SECONDS_PER_CASE, from the GUI's own counter
    sleep 5
    local f0 t0 f1 t1
    f0="$(counter frames.presented_total)"; t0="$(counter published_unix_ms)"
    sleep "$SECONDS_PER_CASE"
    f1="$(counter frames.presented_total)"; t1="$(counter published_unix_ms)"
    [[ -n "$f0" && -n "$f1" && "$t1" -gt "$t0" ]] || { echo -1; return; }
    echo $(( (f1 - f0) * 1000 / (t1 - t0) ))
}

FAILED=0
check() { # name value low high
    if (( $2 >= $3 && $2 <= $4 )); then
        log "ok   $1: $2 (want $3..$4)"
    else
        log "FAIL $1: $2 (want $3..$4)"
        FAILED=1
    fi
}

launch visible || exit 2
visible="$(fps)"
stop_gui
if (( visible < 20 )); then
    log "visible output painted only $visible fps at max_fps=30: macOS is not drawing the test window."
    log "Move it where no opaque window covers it (FT_RR_X / FT_RR_Y; now $FT_RR_X,$FT_RR_Y)."
    exit 2
fi
check "visible output (fps)" "$visible" 20 31

launch hidden_output || exit 2
check "output in a background tab (fps)" "$(fps)" 0 8
stop_gui

launch hidden_title || exit 2
check "title changes in a background tab (fps)" "$(fps)" 0 20
stop_gui

launch visible 10 || exit 2
check "visible output with unfocused_max_fps=10 (fps)" "$(fps)" 8 11
stop_gui

launch gradient || exit 2
sleep 8
atlas_mib=$(( $(counter gpu.textures.atlas.live_bytes) / 1048576 ))
check "maximized gradient window's glyph atlas (MiB)" "$atlas_mib" 1 64
stop_gui

if (( FAILED )); then
    log "FAIL"
    exit 1
fi
log "PASS"
exit 0
