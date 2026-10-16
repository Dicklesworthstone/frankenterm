#!/usr/bin/env bash
# E2E: lossless mux upgrade through the PTY guardian (scripts/mux-guardian-host.sh).
#
#   test_mux_guardian_lossless_upgrade.sh <old family dir> <new family dir> [host script]
#
# Each family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build. Runs in a NEW isolated sandbox (own HOME, XDG dirs, sockets,
# FTG_INSTANCE) with the `direct` service manager, so it never touches a live
# mux, systemd or launchd. Nothing is deleted; every started process is stopped.
#
# Pass criteria (all must hold):
#   1. install: a guardian-backed pane runs the test's tick loop.
#   2. upgrade old -> new: same pane process (pid), same pane ids, new mux
#      binary running, and every line printed before the upgrade readable
#      exactly once, with no gap and no duplicate.
#   3. rollback new -> old: same pane process, pane ids and history again.
#   4. power loss (everything killed) + install: a fresh pane comes up.
#
# SOAK_SECONDS=N adds a soak before the upgrade: the pane first prints a
# 3,000-line burst and then a line a second while recovery captures run
# every 2 s for N seconds. Recovery must publish by halfway and again
# after it, never disable itself, and keep the checkpoint stage at most
# 8 uploads, the pane catalog at most 8 members (the guardian keeps the
# newest 8 so failed captures cannot reclaim a committed root's member), and each output journal
# at most a few segments. Keep N under ~180 so tick 0 stays in the tail.
set -uo pipefail
# The guardian refuses group- or world-writable directories on its paths
# (a umask of 002 on some hosts); keep any stricter umask.
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
SOAK=${SOAK_SECONDS:-0}
OLD=${1:?old family dir}
NEW=${2:?new family dir}
TOOL=${3:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 1; }

case $(uname -s) in Darwin) ROOT=/private/tmp ;; *) ROOT=/tmp ;; esac
S=$ROOT/ftgx-$$
mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; exit 1; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=e2e FTG_RECOVERY_INTERVAL=2
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
cd "$HOME" || exit 1

FAILS=0
step() { echo "== $*"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

STATE=$HOME/.local/state/frankenterm-guardian-e2e
RUN=$XDG_RUNTIME_DIR/frankenterm-guardian-e2e
[ "$(uname -s)" = Darwin ] && RUN=$STATE/run
MARK="FTGX_MARK_$$"
mkdir -p "$XDG_CONFIG_HOME/frankenterm" "$HOME/.ssh"
# Real hosts have an SSH config, and the mux registers a domain per host in
# it; those domains must not stop recovery (they once disabled it for good).
printf 'Host ftgx-e2e-planted-host\n  HostName 127.0.0.1\n' > "$HOME/.ssh/config"
BURST=""
[ "$SOAK" -gt 0 ] && BURST="j=0; while [ \$j -lt 3000 ]; do echo burst \$j; j=\$((j+1)); sleep 0.002; done; "
cat > "$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.toml" <<TOML
default_prog = ["/bin/bash", "-c", "${BURST}i=0; while :; do echo \\"$MARK tick \$i\\"; i=\$((i+1)); sleep 1; done"]
${E2E_MUX_TOML_EXTRA:-}

[[unix_domains]]
name = "unix"
socket_path = "$RUN/mux.sock"
TOML

ft_text() {  # <family dir> -> pane 0 text, whole history
  WEZTERM_UNIX_SOCKET=$RUN/mux.sock "$1/ft" robot get-text 0 --tail 5000 2>/dev/null
}
# <label> <family dir>: every tick printed so far appears exactly once, from
# tick 0 with no gap, and any burst rows still in the history are unique and
# contiguous up to the last one. A restore that kept a stale durable row and
# replayed it again, or dropped one, fails here.
check_history() {
  local text t first last n dups b bn bdups bfirst
  text=$(ft_text "$2")
  t=$(echo "$text" | grep -o "$MARK tick [0-9]*" | awk '{print $3}' | sort -n)
  first=$(echo "$t" | head -1); last=$(echo "$t" | tail -1)
  n=$(echo "$t" | grep -c .); dups=$(echo "$t" | uniq -d | grep -c .)
  if [ "$first" = 0 ] && [ "$n" -ge 6 ] && [ "$n" = $((last + 1)) ] && [ "$dups" = 0 ]; then
    pass "history intact after $1: ticks 0..$last once each"
  else
    fail "history after $1: first=$first last=$last rows=$n duplicated=$dups"
  fi
  [ "$SOAK" -gt 0 ] || return 0
  # get-text is JSON: rows are joined by escaped newlines, so match unanchored.
  b=$(echo "$text" | grep -o 'burst [0-9]*' | awk '{print $2}' | sort -n)
  bn=$(echo "$b" | grep -c .); bdups=$(echo "$b" | uniq -d | grep -c .)
  bfirst=$(echo "$b" | head -1)
  if [ "$bn" -ge 1 ] && [ "$bdups" = 0 ] && [ "$(echo "$b" | tail -1)" = 2999 ] \
    && [ "$bn" = $((3000 - bfirst)) ]; then
    pass "burst history intact after $1: rows $bfirst..2999 once each"
  else
    fail "burst history after $1: rows=$bn first=${bfirst:-none} duplicated=$bdups"
  fi
}
child_pid() { pgrep -f "^/bin/bash -c .*echo \"$MARK" | head -1; }
count() { command ls "$1" 2>/dev/null | grep -c -- "$2"; }

step "install $(basename "$OLD")"
if "$TOOL" install --family "$OLD" --manager direct; then pass "install"; else fail "install"; fi
sleep 4
PID1=$(child_pid)
[ -n "$PID1" ] && pass "pane process $PID1 running" || fail "no pane process"
"$TOOL" status

if [ "$SOAK" -gt 0 ]; then
  step "soak ${SOAK}s"
  LOG=$STATE/logs/mux-server.log
  sleep $(( SOAK / 2 ))
  HALFWAY=$(grep -c "published local generation" "$LOG")
  sleep $(( SOAK - SOAK / 2 ))
  PUBLISHED=$(grep -c "published local generation" "$LOG")
  REJECTED=$(grep -cE "capture_rejected|admission exhausted" "$LOG")
  DISABLED=$(grep -c "disabled=true" "$LOG")
  RECV=$STATE/receiver/guardian-output-v3
  BROK=$STATE/broker/guardian-output-v3
  UPLOADS=$(command ls "$RECV" | grep '^checkpoint-pane-.*candidate\.ftgcp$' | wc -l | tr -d ' ')
  GENESIS=$(command ls "$RECV" | grep '^checkpoint-genesis-.*candidate\.ftgcp$' | wc -l | tr -d ' ')
  MEMBERS=$(count "$RECV" '^checkpoint-catalog-pane-.*\.ftgccommit$')
  RSEG=$(count "$RECV" '\.segment-.*\.ftgout$')
  BSEG=$(count "$BROK" '\.segment-.*\.ftgout$')
  echo "published=$PUBLISHED (halfway $HALFWAY) rejected=$REJECTED disabled=$DISABLED stage_pane_uploads=$UPLOADS stage_genesis=$GENESIS catalog_members=$MEMBERS guardian_segments=$RSEG broker_segments=$BSEG"
  # A busy guardian may refuse a round now and then; what matters is that
  # recovery keeps advancing and never disables itself.
  [ "$HALFWAY" -ge 1 ] && [ "$PUBLISHED" -gt "$HALFWAY" ] \
    && pass "recovery kept publishing ($HALFWAY generations by halfway, $PUBLISHED by the end)" \
    || fail "recovery stalled ($HALFWAY generations by halfway, $PUBLISHED by the end)"
  [ "$DISABLED" = 0 ] && pass "recovery never disabled itself" \
    || fail "recovery disabled itself $DISABLED time(s)"
  [ $((UPLOADS + GENESIS)) -le 8 ] && pass "stage bounded ($UPLOADS pane + $GENESIS genesis uploads)" \
    || fail "stage holds $UPLOADS pane + $GENESIS genesis uploads"
  [ "$MEMBERS" -le 8 ] && pass "catalog bounded ($MEMBERS members)" || fail "catalog holds $MEMBERS members"
  [ "$RSEG" -le 4 ] && [ "$BSEG" -le 4 ] && pass "journals bounded (guardian $RSEG, broker $BSEG segments)" \
    || fail "journal segments: guardian $RSEG, broker $BSEG"
fi

step "upgrade -> $(basename "$NEW")"
UPGRADE_FLAGS=()
# One available build: still prove a real lossless restart onto the same family.
[ "$(cd "$OLD" && pwd -P)" = "$(cd "$NEW" && pwd -P)" ] && UPGRADE_FLAGS=(--force)
if "$TOOL" upgrade --family "$NEW" "${UPGRADE_FLAGS[@]}"; then pass "upgrade reported success"; else fail "upgrade"; fi
PID2=$(child_pid)
[ -n "$PID1" ] && [ "$PID1" = "$PID2" ] && pass "same pane process after upgrade ($PID2)" \
  || fail "pane process changed across upgrade: $PID1 -> ${PID2:-none}"
check_history upgrade "$NEW"

step "rollback"
if "$TOOL" rollback; then pass "rollback reported success"; else fail "rollback"; fi
PID3=$(child_pid)
[ "$PID1" = "$PID3" ] && pass "same pane process after rollback" || fail "pane process changed across rollback"
check_history rollback "$OLD"

step "power loss + reinstall"
for f in "$STATE"/run.*.pid; do kill -KILL "$(cat "$f")" 2>/dev/null; done
for p in $(pgrep -f "$MARK"); do kill -KILL "$p" 2>/dev/null; done
sleep 1
if "$TOOL" install --family "$NEW" --manager direct; then pass "install after power loss"; else fail "install after power loss"; fi
sleep 4
PID4=$(child_pid)
[ -n "$PID4" ] && [ "$PID4" != "$PID1" ] && pass "fresh pane process $PID4 after power loss" \
  || fail "no fresh pane after power loss (${PID4:-none})"

step "teardown"
"$TOOL" stop-all --i-accept-pane-loss
sleep 1
LEFT=$(pgrep -f "$MARK" | tr '\n' ' ')
[ -z "$LEFT" ] && pass "no leftover processes" || fail "leftover processes: $LEFT"

echo "sandbox: $S"
if [ "$FAILS" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL ($FAILS)"; exit 1; fi
