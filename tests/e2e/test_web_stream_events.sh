#!/usr/bin/env bash
# test_web_stream_events.sh — E2E: a real detection reaches GET /stream/events (ft-xxfwy.18/.19)
#
# Private frankenterm-mux-server + `ft watch`, hermetic, in two modes:
#   standalone  `ft web` in its own process, fed by the storage tail
#   inprocess   `ft watch --web`, sharing the watcher's live EventBus
# A zsh pane gets the title `codex` and prints the real Codex usage-limit line; the
# watcher detects codex.usage.reached, which must reach an SSE subscriber that
# connected before the trigger. A second subscriber filtered to another pane must
# receive nothing.
#
# Needs native `ft` (built with the web feature) and `frankenterm-mux-server`:
# FT_BIN_DIR (default target/debug). WEB_STREAM_MODES narrows the modes.
# Artifacts: tests/e2e/artifacts/web-stream/<run>/<mode>.*
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

PASS=0
FAIL=0
check() {
    if [[ "$2" == 0 ]]; then PASS=$((PASS + 1)); echo "PASS $1"
    else FAIL=$((FAIL + 1)); echo "FAIL $1: $3"; fi
}
now() { python3 -c 'import time;print(time.time())'; }

run_mode() {
    local MODE="$1"
    local D SOCK PORT PANE MUXPID CURL CURL_OTHER TRIGGERED ARRIVED
    local PIDS=()
    D=$(mktemp -d /tmp/ftws-XXXXXX)
    mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
    chmod 700 "$D" "$D/.ft" "$D/runtime"
    SOCK="$D/mux.sock"
    PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
    printf '[[unix_domains]]\nname = "ws"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
    printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
    local ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
        "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
        "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
        "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
        "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
        "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
    ft() { env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" "$@"; }

    env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- /bin/zsh -f \
        > "$OUT/$MODE.mux.log" 2>&1 &
    MUXPID=$!; PIDS+=("$MUXPID")
    for _ in $(seq 1 150); do
        grep -q "pid=$MUXPID" "$SOCK.lock" 2> /dev/null && [[ -S "$SOCK" ]] && break
        sleep 0.2
    done
    PANE=$(ft list --json 2> /dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pane_id"])')

    if [[ "$MODE" == inprocess ]]; then
        ft watch --foreground --poll-interval 500 --web --web-port "$PORT" > "$OUT/$MODE.watch.log" 2>&1 &
        PIDS+=("$!")
    else
        ft watch --foreground --poll-interval 500 > "$OUT/$MODE.watch.log" 2>&1 &
        PIDS+=("$!")
        ft web --port "$PORT" > "$OUT/$MODE.web.log" 2>&1 &
        PIDS+=("$!")
    fi
    for _ in $(seq 1 100); do
        curl -s -o /dev/null "http://127.0.0.1:$PORT/health" && break
        sleep 0.2
    done
    sleep 6

    # Subscribe first, then trigger, so the event can only arrive live.
    curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=detections" \
        > "$OUT/$MODE.sse.txt" 2> "$OUT/$MODE.sse.err" &
    CURL=$!; PIDS+=("$CURL")
    curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=detections&pane_id=$((PANE + 1000))" \
        > "$OUT/$MODE.sse-other-pane.txt" 2> /dev/null &
    CURL_OTHER=$!; PIDS+=("$CURL_OTHER")
    sleep 2
    ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$OUT/$MODE.send1.log" 2>&1
    sleep 1
    ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$OUT/$MODE.send2.log" 2>&1
    TRIGGERED=$(now)
    local deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        grep -q "codex.usage.reached" "$OUT/$MODE.sse.txt" 2> /dev/null && break
        sleep 0.2
    done
    ARRIVED=$(now)
    sleep 2

    for pid in "${PIDS[@]}"; do kill "$pid" 2> /dev/null; done
    for pid in "${PIDS[@]}"; do wait "$pid" 2> /dev/null; done
    cp -R "$D/.ft" "$OUT/$MODE.ft-state" 2> /dev/null

    grep -q "codex.usage.reached" "$OUT/$MODE.sse.txt"
    check "$MODE/detection reaches /stream/events" $? "$(head -c 400 "$OUT/$MODE.sse.txt")"
    python3 -c "import sys; l=float(sys.argv[2])-float(sys.argv[1]); print(f'$MODE trigger-to-SSE latency {l:.2f}s'); sys.exit(0 if l < 10 else 1)" "$TRIGGERED" "$ARRIVED"
    check "$MODE/arrives within 10 s of the pane output" $? "too slow"
    grep -q '"schema":"ft.stream.v1"' "$OUT/$MODE.sse.txt"
    check "$MODE/frames carry schema ft.stream.v1" $? "$(head -c 200 "$OUT/$MODE.sse.txt")"
    grep -q '"kind":"ready"' "$OUT/$MODE.sse-other-pane.txt" \
        && ! grep -q "codex.usage.reached" "$OUT/$MODE.sse-other-pane.txt"
    check "$MODE/pane_id filter excludes other panes' detections" $? "$(head -c 400 "$OUT/$MODE.sse-other-pane.txt")"
    local copies
    copies=$(grep -c "codex.usage.reached" "$OUT/$MODE.sse.txt")
    [[ "$copies" == 1 ]]
    check "$MODE/the detection is delivered exactly once" $? "$copies copies"
}

for mode in ${WEB_STREAM_MODES:-standalone inprocess}; do
    run_mode "$mode"
done
echo "web stream e2e: $PASS passed, $FAIL failed (artifacts: $OUT)"
[[ "$FAIL" == 0 ]]
