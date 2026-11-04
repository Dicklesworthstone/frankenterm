#!/usr/bin/env bash
# tests/e2e/test_gui_keeps_windows_across_mux_restart_macos.sh
#
# Native proof for ft-zy6g7: a GUI attached to a guardian-backed local mux
# over a plain unix socket (no proxy_command, as the GUI reaches a launchd
# mux) keeps its window, tabs and panes when that mux is replaced by a
# lossless upgrade and when it crashes and a service manager restarts it.
# Before ft-zy6g7 the socket's EOF detached the domain: every window of it
# closed, and the restored mux came back, if at all, in a new window.
#
#   test_gui_keeps_windows_across_mux_restart_macos.sh [FrankenTerm.app]
#
# The app (default $FT_GUI_APP_PATH or /Applications/FrankenTerm.app) is
# both the GUI under test and, through its MacOS dir, the guardian family
# (mux server, guardian, ft) of the mux. Everything runs in a new sandbox (own
# HOME, XDG dirs, socket, FTG_INSTANCE) with the `direct` service manager: it
# never touches the user's GUI, mux, guardian or launchd. Nothing is deleted;
# every started process is stopped, and the sandbox path is printed.
#
# Pass criteria (all must hold):
#   1. The GUI attaches and shows tabs A, B and C in one window.
#   2. After a lossless upgrade onto the same family (`upgrade --force`), and
#   3. after SIGKILL of the mux and a restart (`install`, as a service manager
#      restarts it): the GUI logs that it rebound to the restored mux, and
#      then shows the same window with the same tab and pane ids, in the same
#      order, with the same titles, and no other window. The ids are the
#      GUI's own; a domain that detaches and reattaches gets new ones.
#   4. A tab opened after both (D) shows its own output next to A, B and C.
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable.
#
# Env knob: FT_KW_RUST_LOG, RUST_LOG for the GUI and the mux (default info).
set -uo pipefail
# The guardian refuses group- or world-writable directories on its paths.
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'keeps-windows: skipping (Darwin-only)\n' >&2
    exit 2
fi
APP=${1:-${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}}
FAM="$APP/Contents/MacOS"
for b in frankenterm-gui frankenterm-mux-server frankenterm-pty-guardian ft; do
    [[ -x "$FAM/$b" ]] || { printf 'keeps-windows: %s has no %s\n' "$APP" "$b" >&2; exit 2; }
done
HERE="$(cd "$(dirname "$0")" && pwd)"
TOOL="$HERE/../../scripts/mux-guardian-host.sh"
[[ -x "$TOOL" ]] || { printf 'keeps-windows: %s is not executable\n' "$TOOL" >&2; exit 2; }

# AF_UNIX paths are limited to 104 bytes on macOS: keep the root short.
S=/private/tmp/ftkw-$$
mkdir -m 700 "$S" || { printf 'keeps-windows: sandbox %s exists\n' "$S" >&2; exit 2; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=e2e FTG_RECOVERY_INTERVAL=2
export RUST_LOG=${FT_KW_RUST_LOG:-info}
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
STATE=$HOME/.local/state/frankenterm-guardian-e2e
SOCK=$STATE/run/mux.sock
E=$S/gui
mkdir -p "$E"
cp "$HERE/fixtures/gui_keeps_windows_across_mux_restart.lua" "$E/gui.lua"
: > "$E/gui.log"
cat > "$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.toml" <<TOML
default_prog = ["/bin/sh", "-c", "printf '\\\\033]0;A\\\\007'; i=0; while :; do echo A tick \$i; i=\$((i+1)); sleep 1; done"]

[[unix_domains]]
name = "unix"
socket_path = "$SOCK"
TOML

FAILS=0
log() { printf '[keeps-windows %s] %s\n' "$(date +%T)" "$*" >&2; }
pass() { log "PASS: $*"; }
fail() { log "FAIL: $*"; FAILS=$((FAILS + 1)); }
gui_pids() { pgrep -f -- "--config-file $E/gui.lua" || true; }
cleanup() {
    gui_pids | xargs kill -9 2>/dev/null
    "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true
    log "sandbox left at $S"
}
trap cleanup EXIT
lines() { grep -c "^$1|" "$E/gui.log"; }
last() { grep "^$1|" "$E/gui.log" | tail -1; }
# "<window>|<gui windows>|<tab:pane:title,...>" of the newest line of a phase.
shown() { last "$1" | cut -d'|' -f2-; }
wait_for() { # <phase> <ERE for what follows "phase|"> <timeout s>
    local i=0
    until last "$1" | grep -qE "^$1\|$2"; do
        i=$((i + 1))
        if ((i > $3 * 4)); then
            log "timed out waiting for $1|$2; last: $(last "$1")"
            return 1
        fi
        sleep 0.25
    done
}
rebinds() { grep -c 'rebound to the restored mux' "$E/gui.err" 2>/dev/null || true; }
wait_rebind() { # <count before> <timeout s>
    local i=0
    until (($(rebinds) > $1)); do
        i=$((i + 1))
        ((i > $2 * 4)) && return 1
        sleep 0.25
    done
}

log "install the guardian-backed mux from $FAM"
"$TOOL" install --family "$FAM" --manager direct >/dev/null || { log "install failed"; exit 2; }
for _ in $(seq 40); do [[ -S "$SOCK" ]] && break; sleep 0.25; done
[[ -S "$SOCK" ]] || { log "the mux socket $SOCK never appeared"; exit 2; }

echo spawn > "$E/phase"
open -g -n --env HOME="$HOME" --env XDG_CONFIG_HOME="$XDG_CONFIG_HOME" \
    --env XDG_DATA_HOME="$XDG_DATA_HOME" --env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    --env FT_KW_HOME="$HOME" --env FT_KW_DIR="$E" --env FT_KW_SOCK="$SOCK" \
    --env FRANKENTERM_LUA_CONFIG=1 --env RUST_LOG="$RUST_LOG" --stderr "$E/gui.err" \
    -a "$APP" --args --config-file "$E/gui.lua" connect e2e
wait_for spawn '[0-9]+\|1\|[0-9]+:[0-9]+:A,[0-9]+:[0-9]+:B,[0-9]+:[0-9]+:C$' 60 \
    || { log "the GUI never showed A,B,C"; exit 2; }
pass "the GUI shows A,B,C: $(shown spawn)"
# Let recovery publish a generation that names B and C.
sleep 8
echo settled > "$E/phase"
wait_for settled '' 10 || exit 2
BEFORE=$(shown settled)

kept_after() { # <phase> <what happened> <rebind count before it>
    local n seen
    if ! wait_rebind "$3" 90; then
        fail "the GUI logged no rebind after $2"
        return
    fi
    sleep 2
    echo "$1" > "$E/phase"
    if ! wait_for "$1" "${BEFORE//|/\\|}$" 30; then
        fail "after $2 the GUI shows $(shown "$1"), not $BEFORE"
        return
    fi
    # Still so a few status updates later.
    n=$(lines "$1")
    sleep 4
    seen=$(shown "$1")
    if (($(lines "$1") <= n)); then
        fail "the GUI stopped reporting after $2"
    elif [[ "$seen" == "$BEFORE" ]]; then
        pass "after $2 the GUI rebound and kept its window, tabs and panes: $seen"
    else
        fail "after $2 the GUI changed to $seen (was $BEFORE)"
    fi
}

R=$(rebinds)
log "lossless upgrade onto the same family"
"$TOOL" upgrade --family "$FAM" --force >/dev/null || fail "the upgrade failed"
kept_after upgraded "the lossless upgrade" "$R"

R=$(rebinds)
MUX=$(cat "$STATE/run.mux-server.pid")
kill -KILL "$MUX" && log "mux $MUX killed"
for _ in $(seq 40); do kill -0 "$MUX" 2>/dev/null || break; sleep 0.25; done
sleep 2
"$TOOL" install --family "$FAM" --manager direct >/dev/null || fail "the mux did not come back"
kept_after restarted "a crash and restart" "$R"

echo more > "$E/phase"
if wait_for more "${BEFORE//|/\\|},[0-9]+:[0-9]+:D$" 30; then
    pass "a tab opened after both shows its own output: $(shown more)"
else
    fail "the GUI never showed a new tab D: $(shown more)"
fi

if ((FAILS == 0)); then
    log "PASS: the GUI kept its window across a lossless upgrade and a crash restart"
    exit 0
fi
log "FAIL: $FAILS check(s) failed; GUI log $E/gui.err"
exit 1
