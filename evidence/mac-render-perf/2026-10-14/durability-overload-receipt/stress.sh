#!/usr/bin/env bash
# ft-yccm0.2.1.6 native stress receipt: a private mux whose only pane floods
# numbered lines at full speed for SECS seconds under a durability queue
# budget of BUDGET_MB. Reports ingest rate (last sequence number the pane
# shows / SECS), the server's overload/gap log lines, and the gap totals
# `ft session list-durable` reads from the pane's manifest.
# usage: stress.sh BIN_DIR BUDGET_MB [SECS]
set -u
BIN_DIR=$(cd "$1" && pwd -P) || exit 2
BUDGET=$2
SECS=${3:-15}
HERE=$(cd "$(dirname "$0")" && pwd -P)
FT="$BIN_DIR/ft"; MUX="$BIN_DIR/frankenterm-mux-server"
D=$(mktemp -d /tmp/ftds.XXXXXX) || exit 2
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/runtime"
SOCK="$D/mux.sock"
printf 'scrollback_durability_queue_max_mb = %s\n[[unix_domains]]\nname = "ds"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$BUDGET" "$SOCK" > "$D/frankenterm.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
ENVV=(
  "PATH=$BIN_DIR:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
  "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
  "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
  "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
  "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false"
  "RUST_LOG=warn"
)
env -i "${ENVV[@]}" "$MUX" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" \
  -- /usr/bin/python3 -I "$HERE/flood_seq.py" > "$D/mux.log" 2>&1 &
MUX_PID=$!
cleanup() {
  kill -TERM "$MUX_PID" 2>/dev/null
  for _ in $(seq 50); do kill -0 "$MUX_PID" 2>/dev/null || break; sleep 0.2; done
  kill -0 "$MUX_PID" 2>/dev/null && kill -9 "$MUX_PID"
  wait "$MUX_PID" 2>/dev/null
  # Never deletes (AGENTS.md RULE 1): the run directory stays for review.
  echo "run directory kept: $D"
}
trap cleanup EXIT
for _ in $(seq 150); do grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null && [ -S "$SOCK" ] && break; sleep 0.2; done
grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null || { echo "mux never started"; tail -5 "$D/mux.log"; exit 2; }
PANE=$(env -i "${ENVV[@]}" "$FT" -c "$D/ft.toml" list --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["pane_id"])')
t0=$(python3 -c 'import time; print(time.time())')
sleep "$SECS"
env -i "${ENVV[@]}" "$FT" -c "$D/ft.toml" get-text "$PANE" --tail 5 > "$D/tail" 2> "$D/tail.err"
t1=$(python3 -c 'import time; print(time.time())')
last=$(grep -oE 'seq [0-9]+' "$D/tail" | tail -1 | cut -d' ' -f2)
python3 -c "import sys; n=int(sys.argv[1] or 0); dt=float(sys.argv[3])-float(sys.argv[2]); print(f'budget {sys.argv[4]} MiB: {n} lines parsed in {dt:.1f} s = {n/dt/1e6:.2f} M lines/s ({n*64/dt/1048576:.0f} MiB/s)')" "${last:-0}" "$t0" "$t1" "$BUDGET"
echo "overload/gap log lines: $(grep -ciE 'overload|durability gap|drained below' "$D/mux.log")"
grep -iE 'overload|drained below' "$D/mux.log" | sed 's/.*\(WARN\|warn\)/WARN/' | cut -c1-220 | head -3
env -i "${ENVV[@]}" "$FT" -c "$D/ft.toml" session list-durable -f json > "$D/durable.json" 2> "$D/durable.err"
python3 -c '
import json,sys
try:
    d=json.load(open(sys.argv[1]))
except Exception as e:
    print("list-durable unreadable:", e, open(sys.argv[2]).read()[-300:]); sys.exit()
panes=d.get("panes", d if isinstance(d,list) else [])
for p in panes:
    print("list-durable pane:", json.dumps({k: p.get(k) for k in ("state", "retained_rows", "durability_gaps", "error")}))
if not panes: print("list-durable:", json.dumps(d)[:400])
' "$D/durable.json" "$D/durable.err"
