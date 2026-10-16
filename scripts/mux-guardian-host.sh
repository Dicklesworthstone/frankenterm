#!/usr/bin/env bash
# mux-guardian-host.sh — run a FrankenTerm mux whose panes survive mux upgrades.
#
# Panes are owned by a long-lived PTY broker + guardian pair
# (`frankenterm-pty-guardian broker-serve` / `serve`). The mux attaches to the
# guardian, publishes periodic encrypted recovery snapshots, and a replacement
# mux binary re-adopts every pane with `--recovery-restore`: same processes,
# same pane ids, same scrollback. Upgrading the mux therefore no longer kills
# the agents running in it.
#
# Usage (on the host):
#   mux-guardian-host.sh install  --family DIR [--accept-pane-loss] [--manager M]
#   mux-guardian-host.sh upgrade  --family DIR [--timeout SECS] [--force]
#   mux-guardian-host.sh rollback
#   mux-guardian-host.sh status
#   mux-guardian-host.sh run-broker | run-guardian | run-mux     (service entry points)
#   mux-guardian-host.sh stop-all --i-accept-pane-loss            (tears everything down)
#
# DIR holds frankenterm-mux-server, frankenterm-pty-guardian and ft from ONE build.
# Manager M: systemd (Linux default), launchd (macOS default), direct (pid files;
# used by tests). FTG_INSTANCE=name runs an isolated instance (own paths, units,
# and mux socket) next to the default one, for testing on a live host.
#
# Invariants:
#   * The broker owns every pane's child process. It is never restarted by this
#     tool except by `stop-all` or `install --accept-pane-loss`; a broker restart
#     ends every pane.
#   * The guardian and broker keep running the family pinned at install
#     (`guardian-family`); `upgrade` replaces only the mux (`mux-current`).
#   * Nothing is deleted. Stale sockets are moved to $STATE/stale-sockets after
#     a connect attempt proves no listener.
set -euo pipefail

SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
INSTANCE=${FTG_INSTANCE:-}
SUFFIX=${INSTANCE:+-$INSTANCE}
OS=$(uname -s)
BASE=${FTG_BASE:-$HOME/.local/lib/frankenterm-guardian$SUFFIX}
STATE=${FTG_STATE:-$HOME/.local/state/frankenterm-guardian$SUFFIX}
CONFIG_DIR=${FTG_CONFIG_DIR:-$HOME/.config/frankenterm}
MUX_TOML=${FTG_MUX_TOML:-$CONFIG_DIR/mux-server$SUFFIX.toml}
RECOVERY_INTERVAL=${FTG_RECOVERY_INTERVAL:-10}

if [ "$OS" = Linux ] && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  RUN=${FTG_RUN:-$XDG_RUNTIME_DIR/frankenterm-guardian$SUFFIX}
else
  RUN=${FTG_RUN:-$STATE/run}
fi
BROKER_SOCK=$RUN/broker.sock
GUARDIAN_SOCK=$RUN/guardian.sock
REC=$STATE/recovery

log() { printf '[%s] %s\n' "$(date '+%F %T')" "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

default_manager() {
  case "$OS" in
    Linux) echo systemd ;;
    Darwin) echo launchd ;;
    *) echo direct ;;
  esac
}
MANAGER=${FTG_MANAGER:-}

# ---------------------------------------------------------------- families ---
family_tag() {  # <dir> -> <version>-<id7>, from the mux server's own --version
  local v ver rev
  v=$("$1/frankenterm-mux-server" --version 2>/dev/null | head -1) || return 1
  # "frankenterm-mux-server 0.15.21 (d2ec2d45aa2e...)"; builds without a git
  # checkout say "(unknown)", so fall back to the binary's content hash.
  ver=$(echo "$v" | awk '{print $2}')
  rev=$(echo "$v" | sed -nE 's/.*\(([0-9a-f]{7})[0-9a-f]*\).*/\1/p')
  if [ -z "$rev" ]; then
    rev=b$( (sha256sum "$1/frankenterm-mux-server" 2>/dev/null || shasum -a 256 "$1/frankenterm-mux-server") | cut -c1-6)
  fi
  [ -n "$ver" ] || return 1
  echo "$ver-$rev"
}

check_family() {  # <dir>
  local d=$1 b
  for b in frankenterm-mux-server frankenterm-pty-guardian ft; do
    [ -x "$d/$b" ] || die "family $d lacks executable $b"
  done
  local m g
  m=$("$d/frankenterm-mux-server" --version | head -1 | awk '{print $2}')
  g=$("$d/frankenterm-pty-guardian" --version | head -1 | awk '{print $2}')
  [ "$m" = "$g" ] || die "family $d mixes versions (mux $m, guardian $g)"
}

publish_family() {  # <src dir> -> echoes installed dir; immutable once published
  local src=$1 tag dest
  check_family "$src"
  tag=$(family_tag "$src") || die "cannot read family version from $src"
  dest=$BASE/families/$tag
  if [ -d "$dest" ]; then
    log "family $tag already published"
  else
    mkdir -p "$BASE/families"
    cp -R "$src" "$dest.partial.$$"
    chmod 0755 "$dest.partial.$$"/frankenterm-mux-server "$dest.partial.$$"/frankenterm-pty-guardian "$dest.partial.$$"/ft
    mv "$dest.partial.$$" "$dest"
    log "published family $tag"
  fi
  echo "$dest"
}

link_target() { [ -L "$1" ] && readlink "$1" || true; }
set_link() {  # <link> <target>, replaced atomically
  ln -sfn "$2" "$1.new.$$"
  # Replace the link itself: a plain `mv` onto a link to a directory moves
  # the new link into that directory and leaves the old link in place.
  if [ "$OS" = Darwin ]; then
    mv -fh "$1.new.$$" "$1"
  else
    mv -fT "$1.new.$$" "$1"
  fi
}
guardian_bin() { echo "$BASE/guardian-family/frankenterm-pty-guardian"; }

# ------------------------------------------------------------------- state ---
private_dir() { mkdir -p "$1"; chmod 0700 "$1"; }

provision_state() {
  local g
  g=$(guardian_bin)
  private_dir "$STATE"; private_dir "$STATE/receiver"; private_dir "$STATE/broker"
  private_dir "$STATE/broker/spawn"; private_dir "$STATE/broker/lease"
  private_dir "$STATE/stale-sockets"; private_dir "$REC"; private_dir "$RUN"
  "$g" provision-token --token-path "$STATE/receiver/token" >/dev/null
  "$g" provision-token --token-path "$STATE/broker/token" >/dev/null

  local ft=$BASE/guardian-family/ft
  if [ ! -e "$REC/ids.env" ]; then
    (umask 077
     {
       echo "FTG_NAMESPACE=$(openssl rand -hex 32)"
       echo "FTG_POLICY=$(openssl rand -hex 32)"
       echo "FTG_ROOT_ID=$(openssl rand -hex 32)"
       echo "FTG_SESSION=$(hostname -s)$SUFFIX"
     } > "$REC/ids.env.partial" && mv "$REC/ids.env.partial" "$REC/ids.env")
    log "recovery identities created"
  fi
  # shellcheck disable=SC1091
  . "$REC/ids.env"
  if [ ! -e "$REC/kek" ]; then
    (umask 077; head -c 32 /dev/urandom > "$REC/kek.partial" && mv "$REC/kek.partial" "$REC/kek")
    log "recovery wrapping key created"
  fi
  prepare_recovery_store
  if [ ! -d "$REC/enroll" ]; then
    (cd "$STATE" && "$ft" snapshot enroll-recovery-key --root "$REC/enroll" --wrapping-key "$REC/kek" \
       --namespace-id "$FTG_NAMESPACE" --policy-id "$FTG_POLICY" --format json >/dev/null) \
      || die "recovery key enrollment failed"
    log "recovery key enrolled"
  fi
}

prepare_recovery_store() {
  [ -d "$REC/store" ] && return 0
  (cd "$STATE" && "$BASE/guardian-family/ft" snapshot prepare-recovery-store --root "$REC/store" \
     --format json >/dev/null) || die "recovery store preparation failed"
  log "recovery store prepared"
}

write_default_mux_toml() {
  [ -e "$MUX_TOML" ] && return 0
  mkdir -p "$CONFIG_DIR"
  local shell=${FTG_SHELL:-${SHELL:-/bin/bash}}
  {
    echo "# Written by mux-guardian-host.sh; edit freely. Panes are spawned by the guardian."
    echo "default_prog = [\"$shell\", \"-l\"]"
    echo "scrollback_lines = 100000"
    echo "mux_output_parser_buffer_size = 524288"
    echo "mux_output_parser_coalesce_delay_ms = 3"
    if [ -n "$INSTANCE" ]; then
      # An isolated instance must not take the default mux socket.
      echo ""
      echo "[[unix_domains]]"
      echo "name = \"unix\""
      echo "socket_path = \"$RUN/mux.sock\""
    fi
  } > "$MUX_TOML"
  log "wrote $MUX_TOML"
}

# ----------------------------------------------------------------- sockets ---
socket_has_listener() {  # <path>
  if command -v nc >/dev/null 2>&1; then nc -U -z "$1" >/dev/null 2>&1; return; fi
  python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$1" 2>/dev/null
}

retire_stale_socket() {  # <path>; only when nothing is listening
  local sock=$1
  [ -e "$sock" ] || return 0
  if socket_has_listener "$sock"; then die "$sock is live; refusing to start a second listener"; fi
  private_dir "$STATE/stale-sockets"
  mv "$sock" "$STATE/stale-sockets/$(basename "$sock").$(date +%s)"
  log "moved stale socket $sock aside"
}

guardian_probe() {  # prints "ready <panes>" or "down"; logs why when down
  # One probe can lose its connection to a guardian busy with output; only
  # report the guardian down after several consecutive failures.
  local out attempt
  for attempt in 1 2 3 4 5; do
    if out=$("$(guardian_bin)" probe --socket-path "$GUARDIAN_SOCK" --token-path "$STATE/receiver/token" 2>&1); then
      echo "ready $(echo "$out" | sed -nE 's/.*panes=([0-9]+).*/\1/p')"
      return 0
    fi
    [ "$attempt" = 5 ] || sleep 0.5
  done
  log "guardian probe failed: $(echo "$out" | tr '\n' ' ' | cut -c1-300)"
  echo down
}

wait_guardian_ready() {  # <timeout secs>
  local deadline=$(( $(date +%s) + $1 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    case $(guardian_probe) in ready*) return 0 ;; esac
    sleep 1
  done
  return 1
}

# ------------------------------------------------------- service entry points ---
run_broker() {
  private_dir "$RUN"
  retire_stale_socket "$BROKER_SOCK"
  exec "$(guardian_bin)" broker-serve --socket-path "$BROKER_SOCK" --token-path "$STATE/broker/token" \
    --spawn-catalog-path "$STATE/broker/spawn" --lease-catalog-path "$STATE/broker/lease"
}

run_guardian() {
  for _ in $(seq 30); do [ -S "$BROKER_SOCK" ] && socket_has_listener "$BROKER_SOCK" && break; sleep 1; done
  retire_stale_socket "$GUARDIAN_SOCK"
  exec "$(guardian_bin)" serve --socket-path "$GUARDIAN_SOCK" --token-path "$STATE/receiver/token" \
    --broker-socket-path "$BROKER_SOCK" --broker-token-path "$STATE/broker/token"
}

snapshot_published() { ls "$REC/store/roots/"*.root >/dev/null 2>&1; }

mux_args() {  # common mux flags, one per line
  # shellcheck disable=SC1091
  . "$REC/ids.env"
  printf '%s\n' --daemonize=false --config-file "$MUX_TOML" \
    --guardian-socket-path "$GUARDIAN_SOCK" --guardian-token-path "$STATE/receiver/token" \
    --recovery-store "$REC/store" --recovery-enrollment "$REC/enroll" --recovery-kek "$REC/kek" \
    --recovery-namespace "$FTG_NAMESPACE" --recovery-policy "$FTG_POLICY" \
    --recovery-session "$FTG_SESSION" --recovery-root-id "$FTG_ROOT_ID" \
    --recovery-interval-seconds "$RECOVERY_INTERVAL"
}

run_mux() {
  wait_guardian_ready 60 || die "guardian not ready at $GUARDIAN_SOCK"
  local probe panes args=()
  probe=$(guardian_probe)
  case "$probe" in ready\ [0-9]*) ;; *) die "guardian not ready at $GUARDIAN_SOCK" ;; esac
  panes=$(echo "$probe" | awk '{print $2}')
  while IFS= read -r a; do args+=("$a"); done < <(mux_args)
  # Restore only when there is something to restore: a published snapshot AND
  # live guardian panes. After a reboot the guardian is empty; a restore would
  # fail on every start, so start fresh and let periodic recovery publish anew.
  if snapshot_published && [ "$panes" -gt 0 ]; then
    log "restoring $panes guardian pane(s) with $(readlink "$BASE/mux-current")"
    args+=(--recovery-restore)
  else
    if snapshot_published && [ "$panes" = 0 ] && [ -z "$(broker_children)" ]; then
      # The snapshot's panes died with the broker (a reboot or a broker
      # restart), so its lineage cannot continue in a new mux incarnation:
      # periodic recovery would refuse its predecessor for good. Set it aside
      # (never deleted) and let recovery start a new lineage.
      local retired
      retired="$REC/store.retired-$(date +%Y%m%dT%H%M%S)"
      mv "$REC/store" "$retired" || die "cannot set aside the recovery store with no live panes"
      log "set aside recovery store whose panes are gone: $retired"
      prepare_recovery_store
    fi
    log "fresh mux start (snapshot=$(snapshot_published && echo yes || echo no), guardian panes=${panes:-0})"
  fi
  exec "$BASE/mux-current/frankenterm-mux-server" "${args[@]}"
}

# ------------------------------------------------------------ service layer ---
unit() {  # logical name -> manager-specific identifier
  case "$MANAGER" in
    systemd) echo "frankenterm-$1$SUFFIX.service" ;;
    launchd) echo "com.frankenterm.$1$SUFFIX" ;;
    direct) echo "$1" ;;
  esac
}

write_services() {
  local host_script=$BASE/bin/mux-guardian-host.sh
  case "$MANAGER" in
    systemd)
      local d=$HOME/.config/systemd/user
      mkdir -p "$d"
      local env="Environment=FTG_INSTANCE=$INSTANCE"
      cat > "$d/$(unit pty-broker).partial" <<EOF
[Unit]
Description=FrankenTerm PTY broker$SUFFIX (owns guardian-backed panes; restarting it ends them)
[Service]
Type=simple
UMask=0077
$env
ExecStart=$host_script run-broker
Restart=no
KillMode=control-group
[Install]
WantedBy=default.target
EOF
      cat > "$d/$(unit pty-guardian).partial" <<EOF
[Unit]
Description=FrankenTerm PTY guardian$SUFFIX
Wants=$(unit pty-broker)
After=$(unit pty-broker)
[Service]
Type=simple
UMask=0077
$env
ExecStart=$host_script run-guardian
Restart=no
[Install]
WantedBy=default.target
EOF
      cat > "$d/$(unit mux-server).partial" <<EOF
[Unit]
Description=FrankenTerm mux server$SUFFIX (guardian-backed)
Wants=$(unit pty-guardian)
After=$(unit pty-guardian)
StartLimitIntervalSec=300
StartLimitBurst=5
[Service]
Type=simple
$env
ExecStart=$host_script run-mux
Restart=on-failure
RestartSec=5
KillMode=mixed
TimeoutStopSec=30
[Install]
WantedBy=default.target
EOF
      local n
      for n in pty-broker pty-guardian mux-server; do mv "$d/$(unit $n).partial" "$d/$(unit $n)"; done
      systemctl --user daemon-reload
      systemctl --user enable "$(unit pty-broker)" "$(unit pty-guardian)" "$(unit mux-server)" >/dev/null 2>&1
      ;;
    launchd)
      local d=$HOME/Library/LaunchAgents n label keep sub
      mkdir -p "$d" "$STATE/logs"
      for n in pty-broker pty-guardian mux-server; do
        label=$(unit $n)
        # Only the mux restarts on failure. A broker or guardian restart cannot
        # recover live PTYs, so launchd must never restart them on its own.
        case $n in
          pty-broker) sub=run-broker; keep="" ;;
          pty-guardian) sub=run-guardian; keep="" ;;
          mux-server) sub=run-mux; keep="<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>" ;;
        esac
        cat > "$d/$label.plist.partial" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$host_script</string><string>$sub</string></array>
  <key>EnvironmentVariables</key><dict><key>FTG_INSTANCE</key><string>$INSTANCE</string></dict>
  <key>RunAtLoad</key><true/>
  $keep
  <key>ProcessType</key><string>Interactive</string>
  <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
  <key>StandardOutPath</key><string>$STATE/logs/$n.log</string>
  <key>StandardErrorPath</key><string>$STATE/logs/$n.log</string>
</dict></plist>
EOF
        mv "$d/$label.plist.partial" "$d/$label.plist"
      done
      ;;
    direct) mkdir -p "$STATE/logs" ;;
  esac
}

svc_active() {  # <logical name>
  case "$MANAGER" in
    systemd) systemctl --user is-active --quiet "$(unit "$1")" ;;
    launchd) launchctl print "gui/$(id -u)/$(unit "$1")" 2>/dev/null | grep -q 'state = running' ;;
    direct) [ -f "$STATE/run.$1.pid" ] && kill -0 "$(cat "$STATE/run.$1.pid")" 2>/dev/null ;;
  esac
}

svc_pid() {  # <logical name> -> main pid or empty
  case "$MANAGER" in
    systemd) systemctl --user show -p MainPID --value "$(unit "$1")" 2>/dev/null | grep -v '^0$' || true ;;
    launchd) launchctl print "gui/$(id -u)/$(unit "$1")" 2>/dev/null | awk '/^\tpid = /{print $3}' || true ;;
    direct) [ -f "$STATE/run.$1.pid" ] && cat "$STATE/run.$1.pid" || true ;;
  esac
}

svc_start() {  # <logical name>
  case "$MANAGER" in
    systemd) systemctl --user start "$(unit "$1")" ;;
    launchd)
      local plist
      plist=$HOME/Library/LaunchAgents/$(unit "$1").plist
      launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null || launchctl kickstart "gui/$(id -u)/$(unit "$1")"
      ;;
    direct)
      local sub
      case $1 in pty-broker) sub=run-broker ;; pty-guardian) sub=run-guardian ;; mux-server) sub=run-mux ;; esac
      FTG_INSTANCE=$INSTANCE nohup "$BASE/bin/mux-guardian-host.sh" "$sub" >> "$STATE/logs/$1.log" 2>&1 < /dev/null &
      echo $! > "$STATE/run.$1.pid"
      ;;
  esac
}

svc_stop() {  # <logical name>
  case "$MANAGER" in
    systemd) systemctl --user stop "$(unit "$1")" ;;
    launchd) launchctl bootout "gui/$(id -u)/$(unit "$1")" 2>/dev/null || true ;;
    direct)
      local pid
      pid=$(svc_pid "$1")
      [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true
      for _ in $(seq 30); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
      ;;
  esac
}

svc_restart_mux() {
  case "$MANAGER" in
    systemd) systemctl --user restart "$(unit mux-server)" ;;
    launchd) launchctl kickstart -k "gui/$(id -u)/$(unit mux-server)" ;;
    direct) svc_stop mux-server; svc_start mux-server ;;
  esac
}

# ------------------------------------------------------------------ checks ---
mux_socket() {
  if [ -n "$INSTANCE" ]; then echo "$RUN/mux.sock"; return; fi
  case "$OS" in
    Linux) echo "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/frankenterm/sock" ;;
    *) echo "$HOME/.local/share/frankenterm/sock" ;;
  esac
}

broker_children() {  # sorted pids of pane processes
  local b
  b=$(svc_pid pty-broker)
  [ -n "$b" ] || return 0
  { pgrep -P "$b" || true; } | sort -n | tr '\n' ' '
}

mux_pane_ids() {  # sorted pane ids reported by the running mux
  local ft=$BASE/mux-current/ft
  (cd "$STATE" && WEZTERM_UNIX_SOCKET=$(mux_socket) "$ft" robot state 2>/dev/null) \
    | python3 -c 'import json,sys
d=json.load(sys.stdin)
data=d.get("data") or []
print(" ".join(str(p["pane_id"]) for p in sorted(data, key=lambda p: p["pane_id"])))' 2>/dev/null || true
}

newest_publication() {  # mtime (epoch) of the newest recovery publication, or 0
  local f newest=0 t
  for f in "$REC/store/objects/"guardian-*.obj; do
    [ -e "$f" ] || continue
    if [ "$OS" = Darwin ]; then t=$(stat -f %m "$f"); else t=$(stat -c %Y "$f"); fi
    [ "$t" -gt "$newest" ] && newest=$t
  done
  echo "$newest"
}

wait_fresh_publication() {  # <after epoch> <timeout>
  local deadline=$(( $(date +%s) + $2 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    [ "$(newest_publication)" -gt "$1" ] && return 0
    sleep 1
  done
  return 1
}

verify_mux() {  # <expected family dir> <expected panes> <expected pane ids> <expected children> <timeout>
  local fam=$1 want_panes=$2 want_ids=$3 want_children=$4 deadline=$(( $(date +%s) + $5 ))
  local pid exe ids children panes
  while [ "$(date +%s)" -lt "$deadline" ]; do
    sleep 2
    svc_active mux-server || continue
    pid=$(svc_pid mux-server); [ -n "$pid" ] || continue
    if [ "$OS" = Linux ]; then exe=$(readlink "/proc/$pid/exe" 2>/dev/null || true)
    else exe=$(lsof -a -p "$pid" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -1 || true); fi
    case "$exe" in "$fam"/*|"$(cd "$fam" && pwd -P)"/*) ;; *) continue ;; esac
    panes=$(guardian_probe | awk '{print $2}')
    ids=$(mux_pane_ids)
    children=$(broker_children)
    if [ "$panes" = "$want_panes" ] && [ "$ids" = "$want_ids" ] && [ "$children" = "$want_children" ]; then
      log "verified: mux pid $pid from $fam, $panes guardian pane(s), pane ids [$ids], same pane processes"
      return 0
    fi
  done
  log "verification failed: mux exe=${exe:-?} panes=${panes:-?}/$want_panes ids=[${ids:-?}]/[$want_ids] children=[${children:-?}]/[$want_children]"
  return 1
}

# -------------------------------------------------------------- operations ---
cmd_install() {
  local family="" accept_loss=0
  while [ $# -gt 0 ]; do
    case $1 in
      --family) family=$2; shift 2 ;;
      --accept-pane-loss) accept_loss=1; shift ;;
      --manager) MANAGER=$2; shift 2 ;;
      *) die "install: unknown option $1" ;;
    esac
  done
  [ -n "$family" ] || die "install: --family DIR required"
  MANAGER=${MANAGER:-$(default_manager)}
  mkdir -p "$BASE/bin" "$STATE"
  chmod 0700 "$STATE"
  echo "$MANAGER" > "$STATE/manager"
  cp "$SELF" "$BASE/bin/mux-guardian-host.sh.new.$$" && chmod 0755 "$BASE/bin/mux-guardian-host.sh.new.$$"
  mv -f "$BASE/bin/mux-guardian-host.sh.new.$$" "$BASE/bin/mux-guardian-host.sh"

  local dest
  dest=$(publish_family "$family")
  [ -L "$BASE/guardian-family" ] || set_link "$BASE/guardian-family" "$dest"
  [ -L "$BASE/mux-current" ] || set_link "$BASE/mux-current" "$dest"
  provision_state
  write_default_mux_toml

  # An existing, unguarded mux (the pre-guardian unit) owns its panes directly:
  # replacing it ends them. That is the one-time cost of moving to the guardian.
  local legacy_active=0
  if [ -z "$INSTANCE" ]; then
    case "$MANAGER" in
      systemd)
        if systemctl --user is-active --quiet frankenterm-mux-server.service && \
           ! systemctl --user cat frankenterm-mux-server.service 2>/dev/null | grep -q 'run-mux'; then
          legacy_active=1
        fi ;;
      launchd)
        if launchctl print "gui/$(id -u)/com.frankenterm.mux-server" >/dev/null 2>&1 && \
           ! grep -q run-mux "$HOME/Library/LaunchAgents/com.frankenterm.mux-server.plist" 2>/dev/null; then
          legacy_active=1
        fi ;;
    esac
  fi
  if [ "$legacy_active" = 1 ] && [ "$accept_loss" != 1 ]; then
    die "an unguarded mux is running; converting it ends its panes. Re-run with --accept-pane-loss (a session dump is taken first)"
  fi
  if [ "$legacy_active" = 1 ]; then
    local dump
    dump=$STATE/pre-guardian-dump-$(date +%Y%m%dT%H%M%S).json
    if (cd "$STATE" && "$dest/ft" session dump --allow-partial --output "$dump" >/dev/null 2>&1); then
      log "saved pre-conversion session dump: $dump"
    else
      log "session dump of the legacy mux failed; continuing (panes end either way)"
    fi
    [ "$MANAGER" = launchd ] && launchctl bootout "gui/$(id -u)/com.frankenterm.mux-server" 2>/dev/null || true
    [ "$MANAGER" = systemd ] && systemctl --user stop frankenterm-mux-server.service || true
    if [ "$MANAGER" = systemd ]; then
      local old=$HOME/.config/systemd/user/frankenterm-mux-server.service
      [ -e "$old" ] && cp -p "$old" "$STATE/frankenterm-mux-server.service.pre-guardian.$(date +%s)"
    fi
  fi

  write_services
  svc_active pty-broker || svc_start pty-broker
  for _ in $(seq 30); do [ -S "$BROKER_SOCK" ] && break; sleep 1; done
  svc_active pty-guardian || svc_start pty-guardian
  wait_guardian_ready 60 || die "guardian did not become ready; see logs"
  if svc_active mux-server; then
    log "mux already running"
  else
    svc_start mux-server
  fi
  local deadline=$(( $(date +%s) + 60 )) ids=""
  while [ "$(date +%s)" -lt "$deadline" ]; do
    ids=$(mux_pane_ids)
    [ -n "$ids" ] && break
    sleep 2
  done
  [ -n "$ids" ] || die "mux did not come up with a pane; see $STATE/logs or the service journal"
  log "install complete: mux $(readlink "$BASE/mux-current"), guardian $(readlink "$BASE/guardian-family"), panes [$ids]"
}

load_manager() {
  MANAGER=${MANAGER:-$(cat "$STATE/manager" 2>/dev/null || default_manager)}
}

cmd_upgrade() {
  local family="" timeout=90 force=0
  while [ $# -gt 0 ]; do
    case $1 in
      --family) family=$2; shift 2 ;;
      --timeout) timeout=$2; shift 2 ;;
      --force) force=1; shift ;;  # restart losslessly even onto the same family
      *) die "upgrade: unknown option $1" ;;
    esac
  done
  [ -n "$family" ] || die "upgrade: --family DIR required"
  load_manager
  [ -L "$BASE/mux-current" ] || die "not installed (no $BASE/mux-current); run install first"
  cp "$SELF" "$BASE/bin/mux-guardian-host.sh.new.$$" && chmod 0755 "$BASE/bin/mux-guardian-host.sh.new.$$"
  mv -f "$BASE/bin/mux-guardian-host.sh.new.$$" "$BASE/bin/mux-guardian-host.sh"

  local dest current
  dest=$(publish_family "$family")
  current=$(readlink "$BASE/mux-current")
  if [ "$current" = "$dest" ] && [ "$force" != 1 ]; then
    log "mux already runs $(basename "$dest")"
    return 0
  fi

  svc_active mux-server || die "mux is not running; refusing to upgrade a stopped mux"
  # A guardian busy with checkpoints and replay closes probes retryably.
  wait_guardian_ready 60 || die "guardian is not ready; refusing"
  local probe panes ids children
  probe=$(guardian_probe)
  case "$probe" in ready\ [0-9]*) ;; *) die "guardian is not ready; refusing" ;; esac
  panes=$(echo "$probe" | awk '{print $2}')
  ids=$(mux_pane_ids)
  children=$(broker_children)
  [ "${panes:-0}" -gt 0 ] || die "guardian holds no panes; nothing to preserve (use install for a fresh start)"
  [ -n "$ids" ] || die "cannot read the running mux's pane ids"
  log "before: $panes guardian pane(s), pane ids [$ids], pane processes [$children]"

  # The restored layout is the last published snapshot: wait for one taken
  # after this point so panes created moments ago are included.
  # A busy guardian can fail a few rounds in a row, so allow several.
  local mark fresh_within
  mark=$(date +%s)
  fresh_within=$(( RECOVERY_INTERVAL * 6 + 60 ))
  wait_fresh_publication "$mark" "$fresh_within" \
    || die "no fresh recovery snapshot within ${fresh_within}s; refusing (see the mux log)"
  log "fresh recovery snapshot published"

  set_link "$BASE/mux-previous" "$current"
  set_link "$BASE/mux-current" "$dest"
  log "switching mux $(basename "$current") -> $(basename "$dest")"
  svc_restart_mux
  if verify_mux "$dest" "$panes" "$ids" "$children" "$timeout"; then
    log "upgrade complete: $(basename "$current") -> $(basename "$dest")"
    return 0
  fi
  log "upgrade verification failed; rolling back to $(basename "$current")"
  set_link "$BASE/mux-current" "$current"
  svc_restart_mux
  if verify_mux "$current" "$panes" "$ids" "$children" "$timeout"; then
    die "upgrade rolled back to $(basename "$current"); panes preserved"
  fi
  die "upgrade AND rollback verification failed; inspect status and logs before acting"
}

cmd_rollback() {
  load_manager
  local prev
  prev=$(link_target "$BASE/mux-previous")
  [ -n "$prev" ] || die "no previous mux family recorded"
  local cur probe panes ids children
  cur=$(readlink "$BASE/mux-current")
  wait_guardian_ready 60 || die "guardian is not ready; refusing"
  probe=$(guardian_probe)
  case "$probe" in ready\ [0-9]*) ;; *) die "guardian is not ready; refusing" ;; esac
  panes=$(echo "$probe" | awk '{print $2}')
  ids=$(mux_pane_ids)
  children=$(broker_children)
  set_link "$BASE/mux-previous" "$cur"
  set_link "$BASE/mux-current" "$prev"
  svc_restart_mux
  verify_mux "$prev" "$panes" "$ids" "$children" 90 || die "rollback verification failed"
  log "rolled back to $(basename "$prev")"
}

cmd_status() {
  load_manager
  echo "instance:        ${INSTANCE:-default} ($MANAGER)"
  echo "guardian family: $(basename "$(link_target "$BASE/guardian-family")")"
  echo "mux current:     $(basename "$(link_target "$BASE/mux-current")")"
  echo "mux previous:    $(basename "$(link_target "$BASE/mux-previous")")"
  local n
  for n in pty-broker pty-guardian mux-server; do
    echo "$n: $(svc_active $n && echo "active pid $(svc_pid $n)" || echo inactive)"
  done
  echo "guardian probe:  $(guardian_probe)"
  echo "pane ids:        [$(mux_pane_ids)]"
  echo "pane processes:  [$(broker_children)]"
  local p
  p=$(newest_publication)
  if [ "$p" -gt 0 ]; then echo "last snapshot:   $(( $(date +%s) - p ))s ago"; else echo "last snapshot:   none"; fi
}

cmd_stop_all() {
  [ "${1:-}" = --i-accept-pane-loss ] || die "stop-all ends every pane; pass --i-accept-pane-loss"
  load_manager
  svc_stop mux-server || true
  svc_stop pty-guardian || true
  svc_stop pty-broker || true
  log "stopped mux, guardian and broker"
}

case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  upgrade) shift; cmd_upgrade "$@" ;;
  rollback) shift; cmd_rollback ;;
  status) shift; cmd_status ;;
  stop-all) shift; cmd_stop_all "$@" ;;
  run-broker) run_broker ;;
  run-guardian) run_guardian ;;
  run-mux) run_mux ;;
  *) sed -n '2,32p' "$0"; exit 2 ;;
esac
