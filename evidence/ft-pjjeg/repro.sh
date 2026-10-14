#!/usr/bin/env bash
# ft-pjjeg repro: a private mux server whose only pane floods output, then
# repeated `ft get-text --tail` reads of that pane. Counts refusals.
# usage: repro.sh BIN_DIR [reads] [flood-cmd]
set -u
BIN_DIR=$(cd "$1" && pwd -P) || exit 2
N=${2:-40}
FLOOD=${3:-'while :; do cat /usr/share/dict/words; done'}
FT="$BIN_DIR/ft"; MUX="$BIN_DIR/frankenterm-mux-server"
D=$(mktemp -d /tmp/ftpj.XXXXXX) || exit 2
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/runtime"
SOCK="$D/mux.sock"
printf '[[unix_domains]]\nname = "pj"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
ENVV=(
  "PATH=$BIN_DIR:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
  "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
  "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
  "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
  "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false"
  "RUST_LOG=${MUX_LOG:-warn}"
)
env -i "${ENVV[@]}" "$MUX" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" \
  -- /bin/sh -c "$FLOOD" > "$D/mux.log" 2>&1 &
MUX_PID=$!
cleanup() {
  kill -TERM "$MUX_PID" 2>/dev/null
  for _ in $(seq 25); do kill -0 "$MUX_PID" 2>/dev/null || break; sleep 0.2; done
  kill -0 "$MUX_PID" 2>/dev/null && kill -9 "$MUX_PID"
  wait "$MUX_PID" 2>/dev/null
  if [ "${KEEP:-0}" = 1 ]; then echo "kept $D"; else rm -rf -- "$D"; fi
}
trap cleanup EXIT
for _ in $(seq 150); do grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null && [ -S "$SOCK" ] && break; sleep 0.2; done
grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null || { echo "mux never started"; tail -5 "$D/mux.log"; exit 2; }
PANE=$(env -i "${ENVV[@]}" "$FT" -c "$D/ft.toml" list --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["pane_id"])')
echo "mux $MUX_PID pane $PANE; flooding for ${WARM:-3} s"
sleep "${WARM:-3}"
ok=0; fail=0; : > "$D/errs"
for i in $(seq "$N"); do
  if env -i "${ENVV[@]}" "$FT" -c "$D/ft.toml" get-text "$PANE" --tail 20 > "$D/out" 2> "$D/err"; then
    ok=$((ok + 1))
  else
    fail=$((fail + 1)); tail -1 "$D/err" >> "$D/errs"
  fi
done
echo "reads ok=$ok fail=$fail (of $N)"
sort "$D/errs" | uniq -c | sort -rn | head -5
echo "server line-read failures: $(grep -c 'owned line read failed' "$D/mux.log")"
grep 'owned line read failed' "$D/mux.log" | sed 's/.*owned line read/owned line read/' | sort | uniq -c | head
ps -o rss=,%cpu= -p "$MUX_PID"
