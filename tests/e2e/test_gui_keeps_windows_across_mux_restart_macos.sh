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
#   5. The lossless upgrade, back within a few seconds, opens no other window
#      at any status update, up to the crash: no "Reconnecting..." window
#      flashes (ft-9zthd).
#   6. The crash outage, held for 6 s (longer than the GUI's 5 s connection
#      window delay), shows a reconnect window while the mux is down, though
#      it comes under the default 30 s after the upgrade, so it counts as the
#      same outage, whose reattach already closed its unseen window (ft-9zthd).
#   7. A background attach (no window, as the auto-connect supervisor makes)
#      of a domain whose proxy takes 1.5 s opens no connection window, and
#   8. one whose proxy takes 8 s shows its connection window, then closes it
#      once attached (ft-h38m4).
#   9. A GUI whose only domain's mux is killed and comes back at once as a new
#      session, which it cannot rebind to, stays up and shows a window saying
#      the mux restarted. The detach empties its mux, and the GUI quits on that.
#      A race the window lost in most runs, so this takes several rounds, each
#      a new GUI on a plain mux and a HOME that remembers no other domain.
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
PLAIN=
cleanup() {
    gui_pids | xargs kill -9 2>/dev/null
    [[ -z "$PLAIN" ]] || kill -9 "$PLAIN" 2>/dev/null
    "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true
    log "sandbox left at $S"
}
trap cleanup EXIT
start_gui() { # <home> <runtime dir> <stderr file>
    open -g -n --env HOME="$1" --env XDG_CONFIG_HOME="$1/.config" \
        --env XDG_DATA_HOME="$1/.local/share" --env XDG_RUNTIME_DIR="$2" \
        --env FT_KW_HOME="$1" --env FT_KW_DIR="$E" --env FT_KW_SOCK="$SOCK" \
        --env FRANKENTERM_LUA_CONFIG=1 --env RUST_LOG="$RUST_LOG" --stderr "$3" \
        -a "$APP" --args --config-file "$E/gui.lua" connect e2e
}
lines() { grep -c "^$1|" "$E/gui.log"; }
last() { grep "^$1|" "$E/gui.log" | tail -1; }
# "<window>|<gui windows>|<tab:pane:title,...>" of the newest line of a phase.
shown() { last "$1" | cut -d'|' -f2-; }
# The most GUI windows any line of a phase showed after its first <n> lines;
# empty when there is no such line.
most_windows() { grep "^$1|" "$E/gui.log" | tail -n +"$(($2 + 1))" | cut -d'|' -f3 | sort -n | tail -1; }
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
start_gui "$HOME" "$XDG_RUNTIME_DIR" "$E/gui.err"
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
N=$(lines settled)
log "lossless upgrade onto the same family"
"$TOOL" upgrade --family "$FAM" --force >/dev/null || fail "the upgrade failed"
kept_after upgraded "the lossless upgrade" "$R"
# From the upgrade up to the crash, so a window that comes late counts too.
MOST=$(printf '%s\n%s\n' "$(most_windows settled "$N")" "$(most_windows upgraded 0)" | sort -n | tail -1)
if [[ -z "$MOST" ]]; then
    fail "the GUI reported nothing during the lossless upgrade"
elif ((MOST == 1)); then
    pass "the lossless upgrade opened no other window ($(($(lines settled) - N + $(lines upgraded))) status updates)"
else
    fail "a reconnect window flashed during the lossless upgrade: $MOST GUI windows"
fi

R=$(rebinds)
N=$(lines upgraded)
MUX=$(cat "$STATE/run.mux-server.pid")
kill -KILL "$MUX" && log "mux $MUX killed"
for _ in $(seq 40); do kill -0 "$MUX" 2>/dev/null || break; sleep 0.25; done
# Down for longer than the GUI's 5 s connection window delay.
sleep 6
"$TOOL" install --family "$FAM" --manager direct >/dev/null || fail "the mux did not come back"
BACK=$SECONDS
wait_rebind "$R" 30 && log "the GUI rebound $((SECONDS - BACK)) s after the mux was back"
kept_after restarted "a crash and restart" "$R"
MOST=$(most_windows upgraded "$N")
if [[ -z "$MOST" ]]; then
    fail "the GUI reported nothing during the crash outage"
elif ((MOST == 2)); then
    pass "the crash outage showed one reconnect window while the mux was down"
else
    fail "the crash outage showed at most $MOST GUI window(s), not its own and one reconnect window"
fi

echo more > "$E/phase"
if wait_for more "${BEFORE//|/\\|},[0-9]+:[0-9]+:D$" 30; then
    pass "a tab opened after both shows its own output: $(shown more)"
else
    fail "the GUI never showed a new tab D: $(shown more)"
fi

# Background attaches, as the auto-connect supervisor makes at GUI startup,
# through a proxy as slow as a remote domain's ssh (ft-h38m4).
connecting() { grep "^$1|" "$E/windows.log" | grep -c 'FrankenTerm: Connecting'; }
# Whether the newest windows.log line of a phase shows a window of a domain.
shows_domain() { grep "^$1|" "$E/windows.log" | tail -1 | grep -qE "^$1\|(.*;)?$2:"; }
background_attach() { # <domain, also the phase>
    local i=0
    echo "$1" > "$E/phase"
    until shows_domain "$1" "$1"; do
        i=$((i + 1))
        if ((i > 30 * 4)); then
            fail "the background attach of $1 never showed its window"
            return 1
        fi
        sleep 0.25
    done
    # A few more status updates, so a lingering connection window shows.
    sleep 2
}
if background_attach fast; then
    if (($(connecting fast) == 0)); then
        pass "a 1.5 s background connect opened no connection window"
    else
        fail "a 1.5 s background connect flashed a connection window ($(connecting fast) status updates)"
    fi
fi
if background_attach slow; then
    if (($(connecting slow) == 0)); then
        fail "an 8 s background connect never showed its connection window"
    elif grep "^slow|" "$E/windows.log" | tail -1 | grep -q 'FrankenTerm: Connecting'; then
        fail "the 8 s background connect left its connection window open"
    else
        pass "an 8 s background connect showed its connection window and closed it once attached"
    fi
fi

# A mux back as a new session cannot be rebound, so its domain detaches, and a
# GUI with no other domain is left with an empty mux. Each round needs a new GUI:
# the last one's window stays open for 120 s, so its mux can no longer empty.
gui_pids | xargs kill -9 2>/dev/null
"$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true
H2=$S/h2
mkdir -p "$H2/.config/frankenterm" "$H2/.local/share" "$S/r2"
chmod 700 "$H2" "$S/r2"
plain_mux() {
    local i=0
    "$FAM/frankenterm-mux-server" --daemonize=false \
        --config-file "$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.toml" >> "$E/plain-mux.log" 2>&1 &
    PLAIN=$!
    # A stale socket file proves nothing; the lock names the mux that holds it.
    until grep -qw "pid=$PLAIN" "$SOCK.lock" 2>/dev/null; do
        i=$((i + 1))
        if ((i > 10 * 20)); then
            log "the plain mux $PLAIN never took $SOCK"
            exit 2
        fi
        sleep 0.05
    done
}
new_session_round() { # <round>
    local phase=fresh$1 gui i=0
    plain_mux
    echo "$phase" > "$E/phase"
    start_gui "$H2" "$S/r2" "$E/$phase.err"
    until shows_domain "$phase" e2e; do
        i=$((i + 1))
        if ((i > 60 * 4)); then
            log "round $1: the GUI never showed the domain"
            exit 2
        fi
        sleep 0.25
    done
    gui=$(gui_pids | head -1)
    # Sooner, the Connecting window's pane can still keep the mux from emptying,
    # and this round proves nothing (a GUI without the fix passed 4 of 4 at 1 s).
    sleep 3
    kill -KILL "$PLAIN"
    wait "$PLAIN" 2>/dev/null
    plain_mux
    i=0
    until grep "^$phase|" "$E/windows.log" | grep -q 'mux restarted'; do
        if ! kill -0 "$gui" 2>/dev/null; then
            fail "round $1: the GUI quit when its mux came back as a new session, with no window saying why"
            return
        fi
        i=$((i + 1))
        if ((i > 20 * 4)); then
            fail "round $1: no window said the mux restarted; the GUI shows $(grep "^$phase|" "$E/windows.log" | tail -1)"
            return
        fi
        sleep 0.25
    done
    # Still up with that window showing, once the detach has run its course.
    sleep 2
    if kill -0 "$gui" 2>/dev/null; then
        pass "round $1: the GUI stayed up and said its mux restarted"
    else
        fail "round $1: the GUI quit after showing that its mux restarted"
    fi
}
for round in 1 2 3 4 5 6; do
    new_session_round "$round"
    gui_pids | xargs kill -9 2>/dev/null
    kill -9 "$PLAIN" 2>/dev/null
    wait "$PLAIN" 2>/dev/null
done

if ((FAILS == 0)); then
    log "PASS: the GUI kept its window across a lossless upgrade and a crash restart, showed connection windows only for slow connects, and stayed up when its mux came back as a new session"
    exit 0
fi
log "FAIL: $FAILS check(s) failed; GUI log $E/gui.err"
exit 1
