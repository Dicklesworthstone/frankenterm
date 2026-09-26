#!/usr/bin/env bash
# test_web_stream_events.sh — E2E: a real detection reaches GET /stream/events (ft-xxfwy.18/.19)
#
# Private frankenterm-mux-server + `ft watch`, hermetic, in two modes:
#   standalone  `ft web` in its own process, fed by the storage tail
#   inprocess   `ft watch --web`, sharing the watcher's live EventBus
# A zsh pane gets the title `codex` and prints the real Codex usage-limit line; the
# watcher detects codex.usage.reached, which must reach an SSE subscriber that
# connected before the trigger. A second subscriber filtered to another pane must
# receive nothing. A client reconnecting with Last-Event-ID resumes from persisted events.
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
# A fixture shape from redactor_coverage_matrix.rs (not a real credential).
FAKE_TOKEN="sk-ant-api03-FGHIJKLMNOPQRSTUVWXYZ1234567890ABCDEFGH"

run_mode() {
    local MODE="$1"
    local D SOCK PORT PANE MUXPID CURL CURL_OTHER TRIGGERED ARRIVED WEB_PID=
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
        WEB_PID=$!; PIDS+=("$WEB_PID")
    fi
    for _ in $(seq 1 100); do
        curl -s -o /dev/null "http://127.0.0.1:$PORT/health" && break
        sleep 0.2
    done
    curl -s "http://127.0.0.1:$PORT/health" > "$OUT/$MODE.health.json" 2> /dev/null
    sleep 6

    # Subscribe first, then trigger, so the event can only arrive live.
    curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=detections" \
        > "$OUT/$MODE.sse.txt" 2> "$OUT/$MODE.sse.err" &
    CURL=$!; PIDS+=("$CURL")
    curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/events?channel=detections&pane_id=$((PANE + 1000))" \
        > "$OUT/$MODE.sse-other-pane.txt" 2> /dev/null &
    CURL_OTHER=$!; PIDS+=("$CURL_OTHER")
    curl -sN --max-time 25 "http://127.0.0.1:$PORT/stream/deltas?pane_id=$PANE&max_hz=20" \
        > "$OUT/$MODE.deltas.txt" 2> /dev/null &
    PIDS+=("$!")
    # Record each frame's ARRIVAL time: ts_ms is stamped when a frame is built,
    # before the rate limiter waits.
    python3 -c '
import sys, time, urllib.request
out = open(sys.argv[2], "w")
try:
    with urllib.request.urlopen(sys.argv[1], timeout=25) as stream:
        deadline = time.time() + 25
        for raw in stream:
            line = raw.decode("utf-8", "replace").rstrip("\n")
            if line.startswith("event: "):
                out.write(f"{int(time.time() * 1000)} {line[7:]}\n"); out.flush()
            if time.time() > deadline:
                break
except Exception:
    pass
' "http://127.0.0.1:$PORT/stream/deltas?pane_id=$PANE&max_hz=2" "$OUT/$MODE.deltas-2hz.arrivals" &
    PIDS+=("$!")
    sleep 2
    ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$OUT/$MODE.send1.log" 2>&1
    sleep 1
    ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$OUT/$MODE.send2.log" 2>&1
    TRIGGERED=$(now)
    # A leaked credential in pane output must never leave the server in clear.
    ft send --no-paste "$PANE" "echo auth=$FAKE_TOKEN status=ok" > "$OUT/$MODE.send3.log" 2>&1
    # A burst of output for the max_hz=2 subscriber.
    ft send --no-paste "$PANE" 'for i in {1..40}; do echo burst-$i; sleep 0.05; done' > "$OUT/$MODE.send4.log" 2>&1
    local deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        grep -q "codex.usage.reached" "$OUT/$MODE.sse.txt" 2> /dev/null && break
        sleep 0.2
    done
    ARRIVED=$(now)
    sleep "${DELTA_SETTLE_SECS:-2}"

    # Resume (ft-emlzp): the detection frame's SSE id is its persisted event id.
    # A client reconnecting with Last-Event-ID one below it gets it replayed;
    # one reconnecting at it gets nothing replayed.
    local EVENT_ID
    EVENT_ID=$(python3 - "$OUT/$MODE.sse.txt" << 'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
for i, line in enumerate(lines):
    if line.startswith("data: ") and "codex.usage.reached" in line:
        ids = [l[4:].strip() for l in lines[max(0, i - 3):i] if l.startswith("id: ")]
        print(ids[-1] if ids else "")
        break
PY
)
    curl -sN --max-time 4 -H "Last-Event-ID: $((${EVENT_ID:-1} - 1))" \
        "http://127.0.0.1:$PORT/stream/events?channel=detections" > "$OUT/$MODE.resume-before.txt" 2> /dev/null
    curl -sN --max-time 4 -H "Last-Event-ID: ${EVENT_ID:-0}" \
        "http://127.0.0.1:$PORT/stream/events?channel=detections" > "$OUT/$MODE.resume-at.txt" 2> /dev/null

    if [[ -n "$WEB_PID" ]]; then
        # Slow client: subscribes to the busiest stream and never reads, while
        # the pane floods. The server must stay bounded and responsive.
        local rss_before rss_after web_proc
        # $! is the subshell running the ft() wrapper; env execs ft in its child.
        web_proc=$(pgrep -P "$WEB_PID" | head -1)
        web_proc=${web_proc:-$WEB_PID}
        rss_before=$(ps -o rss= -p "$web_proc" | tr -d ' ')
        python3 -c '
import socket, sys, time
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
s.sendall(f"GET /stream/deltas?pane_id={sys.argv[2]}&max_hz=100 HTTP/1.1\r\nHost: x\r\n\r\n".encode())
time.sleep(float(sys.argv[3]))
' "$PORT" "$PANE" 20 &
        local slow=$!
        ft send --no-paste "$PANE" 'for i in {1..20000}; do echo flood-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done' \
            > "$OUT/$MODE.send5.log" 2>&1
        sleep 15
        rss_after=$(ps -o rss= -p "$web_proc" | tr -d ' ')
        curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$PORT/health" > "$OUT/$MODE.health-under-slow-client.txt"
        kill "$slow" 2> /dev/null; wait "$slow" 2> /dev/null
        echo "$MODE web RSS ${rss_before} KiB -> ${rss_after} KiB under a non-reading client"
        [[ "$(cat "$OUT/$MODE.health-under-slow-client.txt")" == 200 ]] \
            && (( rss_after - rss_before < 51200 ))
        check "$MODE/a non-reading client leaves the server bounded and responsive" $? \
            "rss ${rss_before}->${rss_after} KiB, health $(cat "$OUT/$MODE.health-under-slow-client.txt")"
    fi

    for pid in "${PIDS[@]}"; do kill "$pid" 2> /dev/null; done
    for pid in "${PIDS[@]}"; do wait "$pid" 2> /dev/null; done
    cp -R "$D/.ft" "$OUT/$MODE.ft-state" 2> /dev/null

    local expected_source=storage_tail
    [[ "$MODE" == inprocess ]] && expected_source=bus
    grep -q "\"event_source\":\"$expected_source\"" "$OUT/$MODE.health.json"
    check "$MODE//health reports event_source=$expected_source" $? "$(cat "$OUT/$MODE.health.json")"
    grep -q "codex.usage.reached" "$OUT/$MODE.sse.txt"
    check "$MODE/detection reaches /stream/events" $? "$(head -c 400 "$OUT/$MODE.sse.txt")"
    python3 -c "import sys; l=float(sys.argv[2])-float(sys.argv[1]); print(f'$MODE trigger-to-SSE latency {l:.2f}s'); sys.exit(0 if l < 10 else 1)" "$TRIGGERED" "$ARRIVED"
    check "$MODE/arrives within 10 s of the pane output" $? "too slow"
    grep -q '"schema":"ft.stream.v1"' "$OUT/$MODE.sse.txt"
    check "$MODE/frames carry schema ft.stream.v1" $? "$(head -c 200 "$OUT/$MODE.sse.txt")"
    grep -q '"kind":"ready"' "$OUT/$MODE.sse-other-pane.txt" \
        && ! grep -q "codex.usage.reached" "$OUT/$MODE.sse-other-pane.txt"
    check "$MODE/pane_id filter excludes other panes' detections" $? "$(head -c 400 "$OUT/$MODE.sse-other-pane.txt")"
    grep -q "status=ok" "$OUT/$MODE.deltas.txt" && ! grep -q "$FAKE_TOKEN" "$OUT/$MODE.deltas.txt"
    check "$MODE/output deltas stream redacts a leaked token" $? "$(grep -o 'auth=[^ \"]*' "$OUT/$MODE.deltas.txt" | head -3 | tr '\n' ' ')"
    python3 - "$OUT/$MODE.deltas-2hz.arrivals" <<'PY'
import sys
stamps = [int(line.split()[0]) for line in open(sys.argv[1]) if line.split()[1:] == ["delta"]]
gaps = [b - a for a, b in zip(stamps, stamps[1:])]
print(f"max_hz=2 delta frames: {len(stamps)}, min spacing {min(gaps) if gaps else 'n/a'} ms")
sys.exit(0 if len(stamps) >= 2 and min(gaps) >= 400 else 1)
PY
    check "$MODE/max_hz=2 spaces delta frames at least ~500 ms apart" $? "see $OUT/$MODE.deltas-2hz.arrivals"
    local copies
    copies=$(grep -c "codex.usage.reached" "$OUT/$MODE.sse.txt")
    [[ "$copies" == 1 ]]
    check "$MODE/the detection is delivered exactly once" $? "$copies copies"
    [[ -n "$EVENT_ID" ]] && grep -q "^id: $EVENT_ID\$" "$OUT/$MODE.sse.txt"
    check "$MODE/the detection frame's SSE id is its persisted event id" $? "id '${EVENT_ID}'"
    # Replayed frames' persisted ids, per resume point. Later detections (ids
    # above the codex one) may legitimately be replayed too.
    replayed_ids() {
        python3 - "$1" << 'PY'
import json, sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    if line.startswith("data: ") and '"replayed":true' in line:
        event = json.loads(line[6:])["data"]["event"]
        print(event.get("event_id"), event.get("detection", {}).get("rule_id"))
PY
    }
    replayed_ids "$OUT/$MODE.resume-before.txt" > "$OUT/$MODE.resume-before.ids"
    replayed_ids "$OUT/$MODE.resume-at.txt" > "$OUT/$MODE.resume-at.ids"
    [[ "$(grep -c "^$EVENT_ID codex.usage.reached\$" "$OUT/$MODE.resume-before.ids")" == 1 ]]
    check "$MODE/Last-Event-ID just before it replays the detection once" $? \
        "$(tr '\n' ' ' < "$OUT/$MODE.resume-before.ids")"
    python3 -c 'import sys;ids=[int(l.split()[0]) for l in open(sys.argv[1]) if l.strip()];sys.exit(0 if all(i > int(sys.argv[2]) for i in ids) else 1)' \
        "$OUT/$MODE.resume-at.ids" "${EVENT_ID:-0}" && grep -q '"kind":"ready"' "$OUT/$MODE.resume-at.txt"
    check "$MODE/Last-Event-ID at it replays nothing at or before it" $? \
        "$(tr '\n' ' ' < "$OUT/$MODE.resume-at.ids")"
    ! grep -q '^id: ' <(grep -B2 '"kind":"ready"' "$OUT/$MODE.resume-at.txt")
    check "$MODE/non-event frames carry no SSE id" $? "$(head -c 200 "$OUT/$MODE.resume-at.txt")"
}

for mode in ${WEB_STREAM_MODES:-standalone inprocess}; do
    run_mode "$mode"
done
echo "web stream e2e: $PASS passed, $FAIL failed (artifacts: $OUT)"
[[ "$FAIL" == 0 ]]
