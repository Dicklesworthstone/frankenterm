#!/usr/bin/env bash
# E2E: scripts/mux-guardian-host.sh under launchd (macOS only).
#
#   test_mux_guardian_launchd.sh <family dir> [host script]
#
# The family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build (an app bundle's Contents/MacOS will do). The other guardian
# e2e tests use the `direct` manager; this one is the only run of the launchd
# code paths, which launchd executes with /bin/bash 3.2 and BSD tools.
#
# It installs an isolated instance (FTG_INSTANCE=ld: labels
# com.frankenterm.*-ld, its own HOME, XDG dirs, state and mux socket), so it
# never touches a live mux; the units still run in this account's real
# launchd domains. Nothing is deleted, and every unit it loads is booted out
# when it ends.
#
# Pass criteria (all must hold):
#   1. install: the broker, guardian and mux run in user/<uid> and not in
#      gui/<uid>; their plists lint, say Background and carry the installer's
#      FT_GRID_ENGINE, and the mux runs with it; the login loader is Aqua.
#   2. re-running install while the units run rewrites the plists (a new
#      FTG_RECOVERY_INTERVAL) and restarts nothing; a forced same-family
#      upgrade then restarts the mux on the rewritten definition, with the
#      same pane ids, and leaves the broker alone.
#   3. with the units gone, as when a GUI teardown took units an older
#      install left in gui/<uid>, launchd running the login loader brings all
#      three back in user/<uid> and the mux serves panes.
#   4. load-units restarts a unit that is loaded but stopped in user/<uid>
#      without printing an error, and names a unit whose plist is missing.
#   5. load-units moves a stopped unit an older install left in gui/<uid>
#      to user/<uid>.
#   6. stop-all returns only once every unit's process has exited, and
#      leaves nothing loaded in either domain.
set -u
[ "$(uname -s)" = Darwin ] || { echo "SKIP: launchd runs only on macOS"; exit 0; }
FAM=${1:?family dir}
TOOL=${2:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 1; }

S=/private/tmp/ftld-$$
mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; exit 1; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=ld FTG_RECOVERY_INTERVAL=5 FT_GRID_ENGINE=page
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE RUST_LOG
cd "$HOME" || exit 1
U=$(id -u)
LA=$HOME/Library/LaunchAgents
STATE=$HOME/.local/state/frankenterm-guardian-ld
UNITS="pty-broker pty-guardian mux-server"
echo "sandbox: $S"

FAILS=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
label() { echo "com.frankenterm.$1-ld"; }
cleanup() {
  "$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1
  local n
  for n in login-loader $UNITS; do
    launchctl bootout "gui/$U/$(label "$n")" 2>/dev/null
    launchctl bootout "user/$U/$(label "$n")" 2>/dev/null
  done
  true
}
trap cleanup EXIT

running_in() { launchctl print "$1/$U/$(label "$2")" 2>/dev/null | grep 'state = running' >/dev/null; }
loaded_in() { launchctl print "$1/$U/$(label "$2")" >/dev/null 2>&1; }
pid_in() { launchctl print "$1/$U/$(label "$2")" 2>/dev/null | awk '/^\tpid = /{print $3}'; }
env_of() { ps eww -o command= -p "$1" | tr ' ' '\n' | grep -x "$2" >/dev/null; }  # <pid> <NAME=value>
# launchd keeps a booted-out label loaded until its process has exited.
none_in_user() { local n; for n in "$@"; do loaded_in user "$n" && return 1; done; return 0; }
pane_ids() { "$TOOL" status 2>/dev/null | sed -n 's/^pane ids: *\[\(.*\)\]$/\1/p'; }
has_panes() { [ -n "$(pane_ids)" ]; }
all_running_in_user() { local n; for n in $UNITS; do running_in user "$n" || return 1; done; }
guardian_stopped() { loaded_in user pty-guardian && ! running_in user pty-guardian; }
wait_for() {  # <secs> <command...>
  local n=$1; shift
  while [ "$n" -gt 0 ]; do "$@" && return 0; sleep 1; n=$((n - 1)); done
  return 1
}

echo "== 1. install --manager launchd"
if "$TOOL" install --family "$FAM" --manager launchd > "$S/install.log" 2>&1; then pass "install"
else fail "install"; tail -20 "$S/install.log"; fi
for n in $UNITS; do
  if running_in user "$n"; then pass "$n runs in user/$U"; else fail "$n is not running in user/$U"; fi
  if loaded_in gui "$n"; then fail "$n is also loaded in gui/$U"; else pass "$n is not in gui/$U"; fi
  p=$LA/$(label "$n").plist
  if plutil -lint "$p" >/dev/null; then pass "$n plist lints"; else fail "$n plist does not lint"; fi
  if [ "$(plutil -extract LimitLoadToSessionType raw "$p" 2>/dev/null)" = Background ]; then
    pass "$n plist is Background"; else fail "$n plist is not Background"; fi
  if [ "$(plutil -extract EnvironmentVariables.FT_GRID_ENGINE raw "$p" 2>/dev/null)" = page ]; then
    pass "$n plist carries FT_GRID_ENGINE=page"; else fail "$n plist lacks FT_GRID_ENGINE"; fi
done
lp=$LA/$(label login-loader).plist
if plutil -lint "$lp" >/dev/null && [ "$(plutil -extract LimitLoadToSessionType raw "$lp")" = Aqua ]; then
  pass "login-loader plist lints and is Aqua"; else fail "login-loader plist"; fi
mux_pid=$(pid_in user mux-server)
if env_of "$mux_pid" FT_GRID_ENGINE=page; then
  pass "mux (pid $mux_pid) runs with FT_GRID_ENGINE=page"; else fail "mux env lacks FT_GRID_ENGINE=page"; fi
ids=$(pane_ids)
if [ -n "$ids" ]; then pass "mux reports panes [$ids]"; else fail "mux reports no panes"; fi
broker_pid=$(pid_in user pty-broker)
guardian_pid=$(pid_in user pty-guardian)

echo "== 2. re-install with a new setting, then a forced same-family upgrade"
export FTG_RECOVERY_INTERVAL=6
if "$TOOL" install --family "$FAM" --manager launchd > "$S/reinstall.log" 2>&1; then pass "re-install"
else fail "re-install"; tail -20 "$S/reinstall.log"; fi
if [ "$(plutil -extract EnvironmentVariables.FTG_RECOVERY_INTERVAL raw "$LA/$(label mux-server).plist")" = 6 ]; then
  pass "re-install rewrote the mux plist"; else fail "the mux plist still has the old interval"; fi
if [ "$(pid_in user mux-server)" = "$mux_pid" ] && env_of "$mux_pid" FTG_RECOVERY_INTERVAL=5 \
   && [ "$(pid_in user pty-guardian)" = "$guardian_pid" ] && [ "$(pid_in user pty-broker)" = "$broker_pid" ]; then
  pass "re-install restarted nothing"; else fail "re-install restarted a unit"; fi
if "$TOOL" upgrade --family "$FAM" --force > "$S/upgrade.log" 2>&1; then pass "upgrade"
else fail "upgrade"; tail -20 "$S/upgrade.log"; fi
new_mux=$(pid_in user mux-server)
if [ -n "$new_mux" ] && [ "$new_mux" != "$mux_pid" ]; then pass "mux restarted ($mux_pid -> $new_mux)"
else fail "mux pid $mux_pid -> '$new_mux'"; fi
if env_of "$new_mux" FTG_RECOVERY_INTERVAL=6; then pass "the restarted mux runs the rewritten plist"
else fail "the restarted mux runs the old definition"; fi
if loaded_in gui mux-server; then fail "mux loaded in gui/$U after the upgrade"; else pass "mux stays in user/$U only"; fi
if [ "$(pane_ids)" = "$ids" ]; then pass "same pane ids [$ids]"; else fail "pane ids [$ids] -> [$(pane_ids)]"; fi
if [ "$(pid_in user pty-broker)" = "$broker_pid" ]; then pass "broker untouched (pid $broker_pid)"
else fail "broker changed"; fi

echo "== 3. units gone (as a GUI teardown takes gui/ units); launchd runs the login loader"
for n in mux-server pty-guardian pty-broker; do launchctl bootout "user/$U/$(label "$n")" 2>/dev/null; done
# shellcheck disable=SC2086
if wait_for 60 none_in_user $UNITS; then pass "units booted out"; else fail "units still loaded"; fi
# launchd gives its jobs the account's HOME, which in production is the
# installer's, where install wrote the plists. Give this sandboxed install's
# loader the sandbox HOME, as production would have it.
mkdir -p "$S/loader"
cp "$lp" "$S/loader/$(label login-loader).plist"
plutil -insert EnvironmentVariables.HOME -string "$HOME" "$S/loader/$(label login-loader).plist"
if launchctl bootstrap "gui/$U" "$S/loader/$(label login-loader).plist"; then pass "login loader loaded into gui/$U"
else fail "login loader bootstrap"; fi
if wait_for 120 all_running_in_user; then pass "the loader brought all three back in user/$U"
else fail "units not all running after the loader"; fi
if wait_for 90 has_panes; then pass "mux serves panes again [$(pane_ids)]"; else fail "mux serves no panes after the loader"; fi
if grep -q 'could not start' "$STATE/logs/login-loader.log" 2>/dev/null; then
  fail "the loader logged: $(grep -m1 'could not start' "$STATE/logs/login-loader.log")"
else pass "the loader logged no failed start"; fi
launchctl bootout "gui/$U/$(label login-loader)" 2>/dev/null

echo "== 4. load-units: a stopped unit in user/ restarts quietly; a missing plist is named"
g1=$(pid_in user pty-guardian)
kill -TERM "$g1" 2>/dev/null
if wait_for 30 guardian_stopped; then pass "guardian (pid $g1) stopped, still loaded"
else fail "guardian did not stop while loaded"; fi
if "$TOOL" load-units > "$S/load-units-4a.log" 2>&1; then pass "load-units exit 0"; else fail "load-units exit"; fi
g2=$(pid_in user pty-guardian)
if [ -n "$g2" ] && [ "$g2" != "$g1" ]; then pass "guardian restarted in user/$U ($g1 -> $g2)"
else fail "guardian pid $g1 -> '$g2'"; fi
if grep -q -E 'Bootstrap failed|could not start' "$S/load-units-4a.log"; then
  fail "load-units was noisy: $(tr '\n' ' ' < "$S/load-units-4a.log")"
else pass "load-units printed no error"; fi
launchctl bootout "user/$U/$(label pty-broker)" 2>/dev/null
wait_for 30 none_in_user pty-broker || fail "the broker label stayed loaded after bootout"
mv "$LA/$(label pty-broker).plist" "$S/broker.plist.aside"
"$TOOL" load-units > "$S/load-units-4b.log" 2>&1
if grep -q "$(label pty-broker).plist is missing" "$S/load-units-4b.log"; then pass "the missing broker plist is named"
else fail "the missing plist is not named: $(tr '\n' ' ' < "$S/load-units-4b.log")"; fi
mv "$S/broker.plist.aside" "$LA/$(label pty-broker).plist"

echo "== 5. a stopped unit an older install left in gui/ moves to user/"
legacy=$S/legacy/$(label pty-broker).plist
mkdir -p "$S/legacy"
cp "$LA/$(label pty-broker).plist" "$legacy"
plutil -remove LimitLoadToSessionType "$legacy"
plutil -replace RunAtLoad -bool false "$legacy"
if launchctl bootstrap "gui/$U" "$legacy"; then pass "legacy broker loaded (stopped) in gui/$U"
else fail "legacy broker bootstrap into gui/"; fi
if "$TOOL" load-units > "$S/load-units-5.log" 2>&1; then pass "load-units exit 0"; else fail "load-units exit"; fi
if running_in user pty-broker; then pass "broker now runs in user/$U"; else fail "broker not running in user/$U"; fi
if loaded_in gui pty-broker; then fail "broker still loaded in gui/$U"; else pass "broker no longer in gui/$U"; fi

echo "== 6. stop-all"
pids=""; for n in $UNITS; do pids="$pids $(pid_in user "$n")"; done
if "$TOOL" stop-all --i-accept-pane-loss > "$S/stop-all.log" 2>&1; then pass "stop-all"; else fail "stop-all"; fi
alive=""; for p in $pids; do kill -0 "$p" 2>/dev/null && alive="$alive $p"; done
if [ -z "$alive" ]; then pass "every unit process ($pids ) had exited when stop-all returned"
else fail "still running after stop-all:$alive"; fi
left=""
for n in $UNITS; do
  loaded_in user "$n" && left="$left user:$n"
  loaded_in gui "$n" && left="$left gui:$n"
done
if [ -z "$left" ]; then pass "nothing loaded after stop-all"; else fail "still loaded:$left"; fi

if [ "$FAILS" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL ($FAILS)"; exit 1; fi
