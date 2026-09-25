#!/usr/bin/env bash
# test_web_stream_events.sh — E2E: a real detection reaches GET /stream/events (ft-xxfwy.18)
#
# Private frankenterm-mux-server + `ft watch` + standalone `ft web`, all hermetic.
# A zsh pane gets the title `codex` and prints the real Codex usage-limit line; the
# watcher persists a codex.usage.reached detection; the standalone web server's
# storage tail must publish it to an SSE subscriber on /stream/events.
#
# Needs native `ft` (built with the web feature) and `frankenterm-mux-server`:
# FT_BIN_DIR (default target/debug). Artifacts: tests/e2e/artifacts/web-stream/<run>/.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
BIN="${FT_BIN_DIR:-${REPO_ROOT}/target/debug}"
FT_BIN="${BIN}/ft"
MUX_BIN="${BIN}/frankenterm-mux-server"
OUT="${SCRIPT_DIR}/artifacts/web-stream/$(date +%Y%m%dT%H%M%S)"
for bin in "$FT_BIN" "$MUX_BIN"; do
    [[ -x "$bin" ]] || { echo "SKIP: $bin not built (set FT_BIN_DIR)" >&2; exit 2; }
done
mkdir -p "$OUT"

D=$(mktemp -d /tmp/ftws-XXXXXX)
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/.ft" "$D/runtime"
SOCK="$D/mux.sock"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
printf '[[unix_domains]]\nname = "ws"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
    "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
    "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
    "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
    "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
    "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
ft() { env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" "$@"; }
PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2> /dev/null; done
    for pid in "${PIDS[@]}"; do wait "$pid" 2> /dev/null; done
    cp -R "$D/.ft" "$OUT/ft-state" 2> /dev/null
}
trap cleanup EXIT

env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- /bin/zsh -f \
    > "$OUT/mux.log" 2>&1 &
MUXPID=$!; PIDS+=("$MUXPID")
for _ in $(seq 1 150); do
    grep -q "pid=$MUXPID" "$SOCK.lock" 2> /dev/null && [[ -S "$SOCK" ]] && break
    sleep 0.2
done
PANE=$(ft list --json 2> /dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pane_id"])')

env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" watch --foreground --poll-interval 500 > "$OUT/watch.log" 2>&1 &
PIDS+=("$!")
env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" web --port "$PORT" > "$OUT/web.log" 2>&1 &
PIDS+=("$!")
for _ in $(seq 1 100); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/health" && break
    sleep 0.2
done
sleep 6

# Subscribe first, then trigger, so the event can only arrive live.
curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=detections" > "$OUT/sse.txt" 2> "$OUT/sse.err" &
CURL=$!; PIDS+=("$CURL")
sleep 2
ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$OUT/send1.log" 2>&1
sleep 1
ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$OUT/send2.log" 2>&1
TRIGGERED=$(python3 -c 'import time;print(time.time())')

deadline=$((SECONDS + 20))
while (( SECONDS < deadline )); do
    grep -q "codex.usage.reached" "$OUT/sse.txt" 2> /dev/null && break
    sleep 0.2
done
ARRIVED=$(python3 -c 'import time;print(time.time())')
kill "$CURL" 2> /dev/null

PASS=0; FAIL=0
check() { if [[ "$2" == 0 ]]; then PASS=$((PASS+1)); echo "PASS $1"; else FAIL=$((FAIL+1)); echo "FAIL $1: $3"; fi; }
grep -q "codex.usage.reached" "$OUT/sse.txt"
check "detection reaches /stream/events" $? "$(head -c 400 "$OUT/sse.txt")"
python3 - "$TRIGGERED" "$ARRIVED" <<'PY'
import sys
latency = float(sys.argv[2]) - float(sys.argv[1])
print(f"trigger-to-SSE latency {latency:.2f}s")
sys.exit(0 if latency < 10 else 1)
PY
check "arrives within 10 s of the pane output" $? "too slow"
echo "web stream e2e: $PASS passed, $FAIL failed (artifacts: $OUT)"
[[ "$FAIL" == 0 ]]
