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
# 3 uploads per pane, each pane catalog at most 8 members (the guardian keeps
# the newest 8 so failed captures cannot reclaim a committed root's member),
# and each output journal at most a few segments. Keep N under ~180 so
# tick 0 stays in the tail.
#
# E2E_PANES=N (default 1) runs N panes; every one must keep its process,
# id and history across the upgrade and the rollback.
#
# E2E_IMAGES=1 (with E2E_PANES > 1) makes every odd pane show an iTerm2
# PNG and a Kitty image, and store a second Kitty image, before it ticks.
# Recovery must still publish (a pane showing an image refused capture
# before graphics checkpoints), and each restore after the upgrade and
# after the rollback must report the images it restored.
#
# E2E_PROTECTED=1 (with E2E_PANES > 2) makes every even pane from 2 up print
# a DECSCA-protected word before it ticks, which leaves the DEC protection
# mode on: recovery must still publish, as a pane holding protection refused
# capture before protection checkpoints.
#
# POST_UPGRADE_SOAK_SECONDS=N (default 0) keeps the upgraded mux running N
# seconds before the rollback; it must publish a recovery generation in
# that time, as a successor whose restore pinned the guardian's catalog
# once never did.
set -uo pipefail
# The guardian refuses group- or world-writable directories on its paths
# (a umask of 002 on some hosts); keep any stricter umask.
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
SOAK=${SOAK_SECONDS:-0}
POST_SOAK=${POST_UPGRADE_SOAK_SECONDS:-0}
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
# Services started directly inherit this shell's environment. An RCH
# worker's RUST_LOG hid the mux's capture-failure warnings, so the
# services log at their own default (info) unless E2E_RUST_LOG names a
# filter (say info,frankenterm_term::line_read=debug).
unset RUST_LOG
[ -n "${E2E_RUST_LOG:-}" ] && export RUST_LOG=$E2E_RUST_LOG
cd "$HOME" || exit 1

FAILS=0
step() { echo "== $*"; }
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

STATE=$HOME/.local/state/frankenterm-guardian-e2e
RUN=$XDG_RUNTIME_DIR/frankenterm-guardian-e2e
[ "$(uname -s)" = Darwin ] && RUN=$STATE/run
# Matched with the following " tick" everywhere, so run 123's pattern never
# matches run 1234's processes.
MARK="FTGX_MARK_$$"
# However the script ends, nothing it started may outlive it.
trap '"$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true' EXIT
mkdir -p "$XDG_CONFIG_HOME/frankenterm" "$HOME/.ssh"
# Real hosts have an SSH config, and the mux registers a domain per host in
# it; those domains must not stop recovery (they once disabled it for good).
printf 'Host ftgx-e2e-planted-host\n  HostName 127.0.0.1\n' > "$HOME/.ssh/config"
BURST=""
[ "$SOAK" -gt 0 ] && BURST="j=0; while [ \$j -lt 3000 ]; do echo burst \$j; j=\$((j+1)); sleep 0.002; done; "
# E2E_PANES=N runs N panes, as a fleet host does: pane 0 as below, pane k
# ticking "$MARK pk tick". The guardian's stage, catalog and replay limits
# are per guardian, so one pane alone never meets them.
PANES=${E2E_PANES:-1}
IMAGES=${E2E_IMAGES:-0}
# A 16x16 PNG; and 2x2 RGBA pixels, for Kitty.
PNG=iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAf8/9hAAADpklEQVR42gXBC0AMBhzA4b8ep8fV5Vacyq10dFKkpRqXHjglIstql0iRmpbHNIWSpJgmLCF5tlqNZZEizwljWVbLGGtjNmOMMcZs9tv3iYjgYC64KQRfayFUKUxWCUlqYb6TkK8R1rsIu7TCZ+7CKZ3wtadww0v4w0eQnv2EPm498PQwI3CgOeMGWRDnbcmcoQoW+/WkeLgVm4Os+XikDc2jbPkiTMmVMXb8Os6eZ+NViP0IQRtsxpBQC0aNVhBttGJ6pA2ZE5TkTbJn3RQHdkxVUx/vyImE3rRP1/DDTGcezHLlvzlaxClOGGAyZ3iigrFJ1kxNUTI7VUVWuppVGU5smqeheqELh7K0nM125/JSHb/kefJ0hReWhT6I6yLBe7EFhiVWTMhVMi3fgYyVjiwr0lCyxpXKEjf2leo4tlHPxU3efL/Fl/vb/Pl3RxDK3QbEY4PwWpklozfb8EaFipTtjry7qy8rq7R8WONBVZ2eg/t8aN3vxzcHArl1yMCfh8MwP2rklRNRiFe9MKJBwfhGJaZmNW+3aFhyXMv7p3RUtHrxyTlfWi4E8OVFA9cuhfNbZwQvLkdjczUW5+smZFibENbek5gOe2Z2ObHgiisrrnmwoduL3TeG0XAriM9vh9Bx18jN+xN59DCWHo8T6PU0Gffn6UjQHSHinhXxDxxIe6Qh+4kbq5/p2fLCl9qXQRyWUM6bR3BVMZk71vE8VyZhpUpDo56P3ikbCbEUJllZM8NWzTx7F5b30lHq6MPOPgHsdw7hZL8ILrnF8KOHiYcDU2BQBirvLF4dmsdQv2LE2F94c4ANqXpH3huspWiInvJhftT4G2gKNHJuxGS+DTZxO3QWf43ORGHMpndkAQMnlBAwqRyZGCIkhtvyztje5Ea480GUN9ujA/k0JpzjsRP5Ki6eblMKvydm8jIpB7uUQvqlluKTXkFwRjUSO02YNUPJomQNhbN1lKX58tFcA42ZEZxZEEvXoiR+XpzBkyXZWOQW4pi/Ht3KSvyLahmzphFJyBHmLrNj6XJn1hZ4sm2VP3tXh3F0bTRt6xK4viGNe2VZ/LO5ANuKUly2VzJ4Vx0jq5qIqmlFksuFhVvtKah0ZeNOL/bsCeJAtZHTtbF07k3mp/r5PG7Iw6yxBHVzBf1bavE73kT4qTNMae1E0g8KOU0q1hzRsvWYD3UnDRw5HcWFsya+O5/O3bZs/m4vxrqjnL5d1Qy60sjr11qJ7O7krRs3+R/oey7fcMgA/AAAAABJRU5ErkJggg==
RGBA=AAECAwQFBgcICQoLDA0ODw==
# E2E_IMAGE_SCROLL=N (with E2E_IMAGES=1) makes image panes also show a
# 512x256 noise PNG (64 by 16 cells) and then print N lines, so its rows
# leave the 1000-line hot tier for durable storage: each such row, its 64
# cells repeating the half-megabyte image, used to exceed what a durable row
# may hold and stall the pane's durability.
SCROLL=${E2E_IMAGE_SCROLL:-0}
if [ "$IMAGES" = 1 ] && [ "$SCROLL" -gt 0 ]; then
  python3 -I -c '
import base64, random, struct, zlib
w, h = 512, 256
noise = random.Random(7)
rows = b"".join(b"\x00" + bytes(noise.getrandbits(8) for _ in range(w * 4)) for _ in range(h))
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 1)) + chunk(b"IEND", b"")
print(base64.b64encode(png).decode(), end="")
' > "$HOME/scroll-image.b64" || { echo "FAIL: generate the scroll image"; exit 1; }
fi
images() {  # -> shell commands showing and storing the images
  echo "printf '\\033]1337;File=inline=1:%s\\007\\n' $PNG; "
  echo "printf '\\033_Ga=T,f=32,s=2,v=2,i=31,q=2;%s\\033\\\\\\n' $RGBA; "
  echo "printf '\\033_Ga=t,f=32,s=2,v=2,i=32,q=2;%s\\033\\\\' $RGBA; "
  # After the pane's birth, as a shell's first output comes: the guardian
  # journals a pane's output a synced record at a time (ft-2pgul), so a
  # birth replaying the half-megabyte image waited for it 15-20 s on a Mac.
  [ "$SCROLL" -gt 0 ] && echo "sleep 3; printf '\\033]1337;File=inline=1:'; cat '$HOME/scroll-image.b64'; printf '\\007\\n'; j=0; while [ \$j -lt $SCROLL ]; do echo scroll \$j; j=\$((j+1)); done; "
}
PROTECTED=${E2E_PROTECTED:-0}
prog() {  # <pane index> -> its shell program
  local tag="" burst="" shown=""
  [ "$1" -gt 0 ] && tag="p$1 "
  [ "$1" = 0 ] && burst=$BURST
  [ "$IMAGES" = 1 ] && [ $(( $1 % 2 )) = 1 ] && shown=$(images | tr -d '\n')
  [ "$PROTECTED" = 1 ] && [ "$1" -ge 2 ] && [ $(( $1 % 2 )) = 0 ] \
    && shown="printf '\\033[1\"qprotected\\033[0\"q\\n'; "
  echo "${burst}${shown}i=0; while :; do echo \"$MARK ${tag}tick \$i\"; i=\$((i+1)); sleep 1; done"
}
if [ "$PANES" -gt 1 ]; then
  # Only a Lua config can open more than one pane at startup (a mux-startup
  # hook, which a --recovery-restore start never runs).
  export FRANKENTERM_LUA_CONFIG=1
  export FTG_MUX_TOML=$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.lua
  {
    echo "local wezterm = require 'wezterm'"
    echo "local progs = {"
    for k in $(seq 0 $((PANES - 1))); do echo "  [==[$(prog "$k")]==],"; done
    echo "}"
    echo "wezterm.on('mux-startup', function()"
    echo "  local _, _, window = wezterm.mux.spawn_window { args = { '/bin/bash', '-c', progs[1] } }"
    echo "  for k = 2, #progs do window:spawn_tab { args = { '/bin/bash', '-c', progs[k] } } end"
    echo "end)"
    echo "return { unix_domains = { { name = 'unix', socket_path = [==[$RUN/mux.sock]==] } } }"
  } > "$FTG_MUX_TOML"
else
  cat > "$XDG_CONFIG_HOME/frankenterm/mux-server-e2e.toml" <<TOML
default_prog = ["/bin/bash", "-c", "${BURST}i=0; while :; do echo \\"$MARK tick \$i\\"; i=\$((i+1)); sleep 1; done"]
${E2E_MUX_TOML_EXTRA:-}

[[unix_domains]]
name = "unix"
socket_path = "$RUN/mux.sock"
TOML
fi

ft_text() {  # <family dir> [pane id] -> that pane's text, whole history
  WEZTERM_UNIX_SOCKET=$RUN/mux.sock "$1/ft" robot get-text "${2:-0}" --tail 5000 2>/dev/null
}
pane_ids() {  # <family dir> -> the mux's pane ids, one per line
  WEZTERM_UNIX_SOCKET=$RUN/mux.sock "$1/ft" robot state 2>/dev/null | python3 -c 'import json,sys
for p in json.load(sys.stdin).get("data") or []: print(p["pane_id"])' 2>/dev/null
}
# <label> <family dir>: every pane but pane 0 (check_history's) shows its own
# ticks exactly once, from 0 with no gap.
check_other_panes() {
  [ "$PANES" -gt 1 ] || return 0
  local id text tag t n last dups good=0
  for id in $(pane_ids "$2"); do
    text=$(ft_text "$2" "$id")
    tag=$(echo "$text" | grep -o "$MARK p[0-9]* tick" | head -1 | awk '{print $2}')
    if [ -z "$tag" ]; then
      # Pane 0 ticks untagged (check_history reads it); any other pane
      # without a tick row lost its history or could not be read.
      echo "$text" | grep -q "$MARK tick" && continue
      fail "pane $id shows no tick rows after $1 (get-text: $(echo "$text" | head -c 240))"
      continue
    fi
    t=$(echo "$text" | grep -o "$MARK $tag tick [0-9]*" | awk '{print $4}' | sort -n)
    n=$(echo "$t" | grep -c .); last=$(echo "$t" | tail -1); dups=$(echo "$t" | uniq -d | grep -c .)
    if [ "$(echo "$t" | head -1)" = 0 ] && [ "$n" = $((last + 1)) ] && [ "$dups" = 0 ]; then
      good=$((good + 1))
    else
      fail "pane $id ($tag) history after $1: rows=$n last=$last duplicated=$dups"
    fi
  done
  [ "$good" = $((PANES - 1)) ] && pass "all $good other panes' histories intact after $1" \
    || fail "only $good of $((PANES - 1)) other panes intact after $1"
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
child_pid() { pgrep -f "^/bin/bash -c .*echo \"$MARK tick" | head -1; }
count() { command ls "$1" 2>/dev/null | grep -c -- "$2"; }
# Restores so far of a checkpoint holding images (the mux logs each one).
# An image pane's checkpoint holds its iTerm PNG and two Kitty images, and
# with E2E_IMAGE_SCROLL the scroll image, read back from durable storage.
SHOWN=3
[ "$SCROLL" -gt 0 ] && SHOWN=4
image_restores() {
  grep -o "restoring a terminal checkpoint holding [0-9]* image" \
    "$STATE/logs/mux-server.log" 2>/dev/null | awk -v shown="$SHOWN" '$6 >= shown' | wc -l | tr -d ' '
}
# <label> <restores before it>: with E2E_IMAGES, every image pane restored its images.
check_image_restores() {
  [ "$IMAGES" = 1 ] || return 0
  local restored=$(( $(image_restores) - $2 ))
  [ "$restored" -ge $(( PANES / 2 )) ] \
    && pass "$1 restored images ($restored checkpoints holding $SHOWN images, $(( PANES / 2 )) image panes)" \
    || fail "$1 restored images from $restored checkpoints holding $SHOWN images for $(( PANES / 2 )) image panes"
}

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
  DISABLED=$(grep -cE "disabled=true|publication lane disabled" "$LOG")
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
  # Each pane holds at most its newest acknowledged upload, one in flight and
  # its Genesis upload (reclaimed only when the stage runs short of room); an
  # acknowledgement reclaims the pane's older uploads. Catalogs and journals
  # are per pane too.
  [ $((UPLOADS + GENESIS)) -le $((3 * PANES)) ] \
    && pass "stage bounded ($UPLOADS pane + $GENESIS genesis uploads, $PANES pane(s))" \
    || fail "stage holds $UPLOADS pane + $GENESIS genesis uploads for $PANES pane(s)"
  [ "$MEMBERS" -le $((8 * PANES)) ] && pass "catalogs bounded ($MEMBERS members, $PANES pane(s))" \
    || fail "catalogs hold $MEMBERS members for $PANES pane(s)"
  [ "$RSEG" -le $((4 * PANES)) ] && [ "$BSEG" -le $((4 * PANES)) ] \
    && pass "journals bounded (guardian $RSEG, broker $BSEG segments, $PANES pane(s))" \
    || fail "journal segments: guardian $RSEG, broker $BSEG for $PANES pane(s)"
fi

step "upgrade -> $(basename "$NEW")"
UPGRADE_FLAGS=()
# One available build: still prove a real lossless restart onto the same family.
[ "$(cd "$OLD" && pwd -P)" = "$(cd "$NEW" && pwd -P)" ] && UPGRADE_FLAGS=(--force)
# bash 3.2 (macOS /bin/bash) calls an empty "${a[@]}" unbound under set -u.
RESTORES=$(image_restores)
if "$TOOL" upgrade --family "$NEW" ${UPGRADE_FLAGS[@]+"${UPGRADE_FLAGS[@]}"}; then
  pass "upgrade reported success"
else
  fail "upgrade"
fi
PID2=$(child_pid)
[ -n "$PID1" ] && [ "$PID1" = "$PID2" ] && pass "same pane process after upgrade ($PID2)" \
  || fail "pane process changed across upgrade: $PID1 -> ${PID2:-none}"
check_history upgrade "$NEW"
check_other_panes upgrade "$NEW"
check_image_restores upgrade "$RESTORES"

if [ "$POST_SOAK" -gt 0 ]; then
  # The successor must go on publishing recovery generations: a catalog
  # member its replays pin once kept it from adopting any checkpoint.
  step "successor soak ${POST_SOAK}s"
  LOG=$STATE/logs/mux-server.log
  BEFORE=$(grep -c "published local generation" "$LOG")
  sleep "$POST_SOAK"
  AFTER=$(grep -c "published local generation" "$LOG")
  REFUSED=$(grep -c "rejected with CapacityExhausted" "$LOG")
  echo "successor published=$((AFTER - BEFORE)) capacity_refusals=$REFUSED"
  [ "$AFTER" -gt "$BEFORE" ] \
    && pass "the successor kept publishing ($((AFTER - BEFORE)) generations in ${POST_SOAK}s)" \
    || fail "the successor published no recovery generation in ${POST_SOAK}s"
fi

step "rollback"
RESTORES=$(image_restores)
if "$TOOL" rollback; then pass "rollback reported success"; else fail "rollback"; fi
PID3=$(child_pid)
[ "$PID1" = "$PID3" ] && pass "same pane process after rollback" || fail "pane process changed across rollback"
check_history rollback "$OLD"
check_other_panes rollback "$OLD"
check_image_restores rollback "$RESTORES"

step "power loss + reinstall"
for f in "$STATE"/run.*.pid; do kill -KILL "$(cat "$f")" 2>/dev/null; done
for p in $(pgrep -f "$MARK "); do kill -KILL "$p" 2>/dev/null; done
sleep 1
if "$TOOL" install --family "$NEW" --manager direct; then pass "install after power loss"; else fail "install after power loss"; fi
sleep 4
PID4=$(child_pid)
[ -n "$PID4" ] && [ "$PID4" != "$PID1" ] && pass "fresh pane process $PID4 after power loss" \
  || fail "no fresh pane after power loss (${PID4:-none})"

step "teardown"
"$TOOL" stop-all --i-accept-pane-loss
# Pane processes, and the broker, guardian and mux (their paths are in $S).
# stop-all waits for the services; a pane's shell learns its PTY closed
# (SIGHUP) only after the broker is gone, so give stragglers 10 s, and name
# whatever is still alive then.
LEFT=""
for _ in $(seq 20); do
  LEFT=$({ pgrep -f "$MARK "; pgrep -f -- "$S/"; } | sort -u | tr '\n' ' ')
  [ -z "$LEFT" ] && break
  sleep 0.5
done
if [ -z "$LEFT" ]; then
  pass "no leftover processes"
else
  fail "leftover processes: $LEFT"
  for p in $LEFT; do ps -o pid=,ppid=,etime=,command= -p "$p" 2>/dev/null || true; done
fi

echo "sandbox: $S"
if [ "$FAILS" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL ($FAILS)"; exit 1; fi
