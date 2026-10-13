#!/usr/bin/env bash
# tests/e2e/test_gui_tab_order_reopen_macos.sh
#
# Native proof that the GUI restores the tab order the user arranged in a
# remote mux window after the GUI is closed (or crashes) and reopened. The
# server never learns about GUI-side reorders, so this order lives only in
# the GUI's layout store (window-state.json, frontend.rs LayoutLifecycle).
#
# What this script does, all under a throwaway HOME (never the user's
# profile, GUI or mux servers):
#   1. Starts a throwaway frankenterm-mux-server (unless FT_TO_PROXY points
#      at a remote one) with one tab titled A.
#   2. Launches the bundled GUI in the background (`open -g`), attached to
#      that mux through a unix_domains proxy_command (the remote-mux shape).
#      Its Lua config (fixtures/gui_tab_order_reopen.lua) spawns tabs B, C, D,
#      then rearranges A,B,C,D into D,A,C,B with C active.
#   3. Kills the GUI with SIGKILL (default; crash path) or quits it.
#   4. A probe GUI with an empty profile confirms the server still has
#      A,B,C,D.
#   5. Relaunches the GUI the way the Dock does (`start`, the domain comes
#      back through remembered-domain auto-connect) and asserts D,A,C,B with
#      C active.
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable.
#
# Env knobs:
#   FT_GUI_APP_PATH  FrankenTerm.app to test (default /Applications/FrankenTerm.app)
#   FT_TO_QUIT       kill9 (default) or quit
#   FT_TO_SPACED     1 = reorder at most once per 2 s; default fires the
#                    reorders back to back like key repeat (the harder case)
#   FT_TO_PROXY      shell command for the domain's proxy_command instead of the
#                    local throwaway mux. Legacy codec-46 check used on
#                    2026-10-12: run the fleet's May frankenterm-mux-server on a
#                    host with no live mux, under its own HOME and
#                    XDG_RUNTIME_DIR (systemd-run --user, so it outlives ssh),
#                    then FT_TO_PROXY="ssh -o BatchMode=yes HOST 'nc -N -U /tmp/X/run/frankenterm/sock'".
#                    Restart that mux between runs; spawned tabs accumulate.
#   FT_TO_LUA_ATTACH 1 = the relaunch also attaches the domain from a Lua
#                    gui-startup timer after opening a local window (the
#                    author's config does this on top of auto-connect)
#   FT_TO_KEEP       1 = keep the scratch directory for inspection
set -uo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'tab-order-reopen: skipping (Darwin-only)\n' >&2
    exit 2
fi
APP="${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}"
BIN="$APP/Contents/MacOS"
[[ -x "$BIN/frankenterm-gui" && -x "$BIN/frankenterm-mux-server" ]] || {
    printf 'tab-order-reopen: %s is not a FrankenTerm bundle\n' "$APP" >&2
    exit 2
}
HERE="$(cd "$(dirname "$0")" && pwd)"
# AF_UNIX paths are limited to 104 bytes on macOS: keep the root short.
E="$(mktemp -d /tmp/ftto.XXXXXX)"
mkdir -p "$E/mux" "$E/gui"
cp "$HERE/fixtures/gui_tab_order_reopen.lua" "$E/gui.lua"
: > "$E/order.log"
MUX_PID=""

log() { printf '[tab-order-reopen %s] %s\n' "$(date +%T)" "$*" >&2; }
gui_pids() { pgrep -f -- "--config-file $E/gui.lua" || true; }
launch_gui() { # home [gui args...]
    local home=$1
    shift
    open -g -n --env HOME="$home" --env FT_TO_HOME="$home" --env FT_TO_DIR="$E" \
        ${FT_TO_PROXY:+--env "FT_TO_PROXY=$FT_TO_PROXY"} \
        ${FT_TO_SPACED:+--env "FT_TO_SPACED=$FT_TO_SPACED"} \
        ${FT_TO_LUA_ATTACH:+--env "FT_TO_LUA_ATTACH=$FT_TO_LUA_ATTACH"} \
        --env FRANKENTERM_LUA_CONFIG=1 --stderr "$home/gui.err" \
        -a "$APP" --args --config-file "$E/gui.lua" "$@"
}
wait_for() { # fixed-string pattern timeout_s
    local i=0
    until grep -qF -- "$1" "$E/order.log"; do
        i=$((i + 1))
        if ((i > $2 * 4)); then
            log "timed out waiting for '$1'; last seen: $(tail -1 "$E/order.log")"
            return 1
        fi
        sleep 0.25
    done
}
quit_guis() {
    [[ -z "$(gui_pids)" ]] && return 0
    echo "quit-$RANDOM" > "$E/phase"
    for _ in $(seq 40); do [[ -z "$(gui_pids)" ]] && return 0; sleep 0.25; done
    gui_pids | xargs kill -9 2>/dev/null
}
cleanup() {
    quit_guis
    [[ -n "$MUX_PID" ]] && kill "$MUX_PID" 2>/dev/null
    if [[ "${FT_TO_KEEP:-0}" == 1 ]]; then log "kept $E"; else rm -rf -- "$E"; fi
}
trap cleanup EXIT

if [[ -z "${FT_TO_PROXY:-}" ]]; then
    HOME="$E/mux" "$BIN/frankenterm-mux-server" --skip-config --daemonize false \
        -- /bin/sh -c "sleep 0.5; printf '\\033]0;A\\007'; exec sleep 100000" \
        > "$E/mux.log" 2>&1 < /dev/null &
    MUX_PID=$!
    for _ in $(seq 40); do [[ -S "$E/mux/.local/share/frankenterm/sock" ]] && break; sleep 0.25; done
    [[ -S "$E/mux/.local/share/frankenterm/sock" ]] || { log "throwaway mux did not start"; exit 2; }
fi

echo spawn > "$E/phase"
launch_gui "$E/gui" connect e2e
wait_for 'spawn|A,B,C,D|D' 60 || exit 2
echo move > "$E/phase"
wait_for 'move|D,A,C,B|C' 30 || { log "the GUI did not apply the reorders"; exit 2; }
sleep 4
if [[ "${FT_TO_QUIT:-kill9}" == kill9 ]]; then
    gui_pids | xargs kill -9
    log "arranged D,A,C,B (C active), then SIGKILLed the GUI"
else
    quit_guis
    log "arranged D,A,C,B (C active), then quit the GUI"
fi
sleep 1

echo probe > "$E/phase"
mkdir -p "$E/probe"
launch_gui "$E/probe" connect e2e
wait_for 'probe|' 60 || exit 2
sleep 2
server=$(grep '^probe|' "$E/order.log" | tail -1)
quit_guis
[[ "$server" == "probe|A,B,C,D|A" ]] || { log "unexpected server-side order: $server"; exit 2; }

echo observe > "$E/phase"
launch_gui "$E/gui" start
wait_for 'observe|' 60 || exit 1
sleep 5
seen=$(grep '^observe|' "$E/order.log" | tail -1)
quit_guis
if [[ "$seen" == "observe|D,A,C,B|C" ]]; then
    log "PASS: reopen restored D,A,C,B with C active (server order A,B,C,D)"
    exit 0
fi
log "FAIL: reopen showed ${seen#observe|} (expected D,A,C,B|C)"
exit 1
