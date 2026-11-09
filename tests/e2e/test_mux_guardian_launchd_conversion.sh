#!/bin/bash
# E2E (macOS, real launchd): install converts an unguarded mux unit to the guardian.
#
#   test_mux_guardian_launchd_conversion.sh <family dir> [host script]
#
# The family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build. An unguarded mux of that family runs in gui/<uid> under the
# label the install's mux takes, as the pre-guardian com.frankenterm.mux-server
# unit runs on a Mac (Aqua, KeepAlive, its own config, its HOME's default
# socket, a pane). Here that label is an instance's (FTG_INSTANCE=cv), so the
# test never sees the operator's own mux; the host script treats both alike.
# Sandbox: /private/tmp/ftcv-<pid> (own HOME and XDG dirs). Nothing is deleted;
# every unit is booted out at the end.
#
# Pass criteria (all must hold):
#   1. install without --accept-pane-loss refuses and leaves the old mux running;
#   2. with it: a session dump holding the old mux's pane, complete; a copy of
#      the old plist, byte for byte, in the install's state dir; the old label
#      gone from gui/<uid> and its process exited; broker, guardian and mux
#      running in user/<uid>, the label's plist now the host script's; the
#      guarded mux serves a pane, whose program has the installer's LANG.
set -u
[ "$(uname -s)" = Darwin ] || { echo "SKIP: launchd runs only on macOS"; exit 0; }
FAM=${1:?family dir}
TOOL=${2:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 1; }
S=/private/tmp/ftcv-$$
mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; exit 1; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=cv FTG_RECOVERY_INTERVAL=5 FT_GRID_ENGINE=page LANG=en_US.UTF-8
mkdir -p "$HOME/Library/LaunchAgents" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE RUST_LOG
cd "$HOME" || exit 1
U=$(id -u)
LA=$HOME/Library/LaunchAgents
L=com.frankenterm.mux-server-cv
STATE=$HOME/.local/state/frankenterm-guardian-cv
echo "sandbox: $S"
# Every pane's program records the locale it starts with, then waits: ps
# shows no environment for the processes the broker starts.
printf '#!/bin/sh\nprintf "LANG=%%s\\n" "${LANG-}" >> %s/pane-locale.txt\nexec /bin/sleep 100000\n' "$S" > "$S/pane.sh"
chmod 755 "$S/pane.sh"
export FTG_SHELL=$S/pane.sh

FAILS=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
cleanup() {
  "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1
  local n
  for n in login-loader pty-broker pty-guardian mux-server; do
    launchctl bootout "gui/$U/com.frankenterm.$n-cv" 2>/dev/null
    launchctl bootout "user/$U/com.frankenterm.$n-cv" 2>/dev/null
  done
  true
}
trap cleanup EXIT
loaded_in() { launchctl print "$1/$U/$2" >/dev/null 2>&1; }
running_in() { launchctl print "$1/$U/$2" 2>/dev/null | grep 'state = running' >/dev/null; }
pid_in() { launchctl print "$1/$U/$2" 2>/dev/null | awk '/^\tpid = /{print $3}'; }
wait_for() {  # <secs> <command...>
  local n=$1; shift
  while [ "$n" -gt 0 ]; do "$@" && return 0; sleep 1; n=$((n - 1)); done
  return 1
}

echo "== the unguarded mux unit in gui/$U"
# Its own config, without unix_domains: it listens on its HOME's default
# socket, as the pre-guardian unit does (the install writes mux-server-cv.toml).
printf 'scrollback_lines = 100000\n' > "$S/legacy-mux.toml"
cat > "$LA/$L.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$L</string>
  <key>ProgramArguments</key><array>
    <string>$FAM/frankenterm-mux-server</string><string>--daemonize=false</string>
    <string>--config-file=$S/legacy-mux.toml</string><string>--cwd=$HOME</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>$HOME</string><key>LANG</key><string>en_US.UTF-8</string>
    <key>FT_GRID_ENGINE</key><string>page</string><key>SHELL</key><string>$S/pane.sh</string>
    <key>XDG_DATA_HOME</key><string>$XDG_DATA_HOME</string><key>XDG_CONFIG_HOME</key><string>$XDG_CONFIG_HOME</string>
  </dict>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>WorkingDirectory</key><string>$HOME</string>
  <key>StandardOutPath</key><string>$S/legacy.log</string>
  <key>StandardErrorPath</key><string>$S/legacy.log</string>
</dict></plist>
EOF
cp -p "$LA/$L.plist" "$S/legacy.plist.orig"
if launchctl bootstrap "gui/$U" "$LA/$L.plist"; then pass "unguarded unit loaded in gui/$U"; else fail "bootstrap of the unguarded unit"; fi
if wait_for 30 running_in gui "$L"; then pass "unguarded mux runs (pid $(pid_in gui "$L"))"; else fail "unguarded mux not running"; fi
legacy_pid=$(pid_in gui "$L")
if wait_for 30 test -S "$XDG_DATA_HOME/frankenterm/sock"; then pass "it listens on its default socket"
else fail "no socket at $XDG_DATA_HOME/frankenterm/sock; see $S/legacy.log"; fi
if wait_for 20 test -s "$S/pane-locale.txt"; then pass "it runs a pane"; else fail "it has no pane"; fi

echo "== 1. install without --accept-pane-loss"
if "$TOOL" install --family "$FAM" --manager launchd > "$S/install-refused.log" 2>&1; then
  fail "install converted the unguarded mux without --accept-pane-loss"
elif grep -q 'an unguarded mux is running' "$S/install-refused.log"; then
  pass "install refuses without --accept-pane-loss"
else fail "install failed otherwise: $(tail -2 "$S/install-refused.log")"; fi
if running_in gui "$L" && [ "$(pid_in gui "$L")" = "$legacy_pid" ]; then pass "the refusal left the unguarded mux running"
else fail "the refusal changed the unguarded mux"; fi

echo "== 2. install --accept-pane-loss"
if "$TOOL" install --family "$FAM" --manager launchd --accept-pane-loss > "$S/install.log" 2>&1; then pass "install"
else fail "install"; tail -20 "$S/install.log"; fi
dump=$(/bin/ls "$STATE"/pre-guardian-dump-*.json 2>/dev/null | head -1)
if [ -n "$dump" ]; then
  shape=$(python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1]))["payload"]; print(d["complete"], len(d["panes"]), len(d["errors"]))' "$dump")
  if [ "$shape" = "True 1 0" ]; then pass "the session dump holds the old mux's pane, complete"
  else fail "session dump (complete, panes, errors) = $shape"; fi
else fail "no session dump: $(grep -i dump "$S/install.log" | head -1)"; fi
kept=$(/bin/ls "$STATE/$L.plist.pre-guardian."* 2>/dev/null | head -1)
if [ -n "$kept" ] && cmp -s "$kept" "$S/legacy.plist.orig"; then pass "the old plist is kept, byte for byte, as $kept"
else fail "no exact copy of the old plist in $STATE (found: '${kept}')"; fi
if loaded_in gui "$L"; then fail "the old label is still loaded in gui/$U"; else pass "the old label is gone from gui/$U"; fi
if kill -0 "$legacy_pid" 2>/dev/null; then fail "the unguarded mux $legacy_pid still runs"; else pass "the unguarded mux exited"; fi
for n in pty-broker pty-guardian mux-server; do
  if running_in user "com.frankenterm.$n-cv"; then pass "$n runs in user/$U"; else fail "$n is not running in user/$U"; fi
done
if grep -q run-mux "$LA/$L.plist"; then pass "the label's plist is now the host script's"; else fail "the label's plist was not rewritten"; fi
ids=$("$TOOL" status 2>/dev/null | sed -n 's/^pane ids: *\[\(.*\)\]$/\1/p')
if [ -n "$ids" ]; then pass "the guarded mux serves panes [$ids]"; else fail "the guarded mux serves no pane"; fi
for _ in $(seq 20); do [ "$(wc -l < "$S/pane-locale.txt")" -ge 2 ] && break; sleep 0.5; done
last=$(sed -n 2p "$S/pane-locale.txt")
if [ "$last" = LANG=en_US.UTF-8 ]; then pass "the guarded pane's program runs with LANG=en_US.UTF-8"
else fail "the guarded pane's program runs with '${last:-no record}'"; fi

if [ "$FAILS" = 0 ]; then echo "RESULT: PASS"; exit 0; fi
echo "RESULT: FAIL ($FAILS)"; exit 1
