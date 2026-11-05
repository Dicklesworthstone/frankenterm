#!/usr/bin/env bash
# Native proof for ft-0rlfq.4: ft tx reconcile settles a step that a crash left
# Pending, and ft tx run then recovers it without sending again.
#
#   test_tx_reconcile_after_crash_macos.sh [bin dir with ft and frankenterm-mux-server]
#
# A private frankenterm-mux-server runs one pane that logs every byte it is sent.
# ft tx run reaches the mux through a relay that forwards the send_text request,
# so the text really reaches the pane, and then drops every reply. ft is left
# waiting after it dispatched and before it records an outcome, and is killed
# there with SIGKILL: the crash ft tx reconcile exists for.
#
# Pass criteria (all must hold):
#   1. ft was still waiting for the send's reply when it was killed, and the pane
#      has the text once.
#   2. ft tx run refuses the Pending step, and its hint names ft tx reconcile for it.
#   3. ft tx rollback refuses it too, with a hint that points at ft tx reconcile;
#      neither sends anything.
#   4. ft tx reconcile without a code prints an allow-once code.
#   5. A wrong code is refused.
#   6. With the code it settles the step as success and writes a receipt.
#   7. The same code a second time is refused.
#   8. ft tx run then completes the transaction and the pane still has the text once.
#
# Exit codes: 0 pass, 1 regression, 2 environment unusable. Nothing is deleted;
# the sandbox path is printed.
set -uo pipefail
BIN=${1:-${FT_BIN_DIR:-/Applications/FrankenTerm.app/Contents/MacOS}}
FT_BIN=$BIN/ft
MUX_BIN=$BIN/frankenterm-mux-server
for b in "$FT_BIN" "$MUX_BIN"; do
    [[ -x "$b" ]] || { echo "tx-reconcile: no $b" >&2; exit 2; }
done
D=$(mktemp -d /private/tmp/fttxr-XXXXXX) || exit 2
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp" "$D/out"
chmod 700 "$D" "$D/.ft" "$D/runtime"
SOCK=$D/mux.sock
RELAY=$D/relay.sock
OUT=$D/out
PASS=0 FAIL=0
log() { printf '[tx-reconcile %s] %s\n' "$(date +%T)" "$*"; }
check() { # name status detail
    if [[ "$2" == 0 ]]; then PASS=$((PASS + 1)); log "PASS: $1"; else FAIL=$((FAIL + 1)); log "FAIL: $1 ${3:-}"; fi
}
is() { [[ "$1" == "$2" ]]; }
json_is() { # <file> <python expression over the parsed JSON d>
    /usr/bin/python3 -c 'import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
sys.exit(0 if eval(sys.argv[2]) else 1)' "$1" "$2"
}
MARKER=$(LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 20)

printf '[[unix_domains]]\nname = "txr"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$RELAY" > "$D/ft-relay.toml"
ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "USER=e2e-operator" "HOME=$D/home"
    "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
    "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
    "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
    "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
ft() { env -i "${ENVV[@]}" "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK" "$FT_BIN" -c "$D/ft.toml" "$@"; }

# The pane: an OSC 133 prompt (so policy sees prompt evidence) that appends
# every byte it reads to pane-input.log.
cat > "$D/pane.py" <<'PY'
import os, select, sys, tty
log = open(sys.argv[1], "ab", buffering=0)
tty.setraw(sys.stdin.fileno())
os.write(1, b'\x1b]133;A\x07$ ')
while True:
    r, _, _ = select.select([0], [], [], 600)
    if not r:
        break
    data = os.read(0, 4096)
    if not data:
        break
    log.write(data)
    if b'\r' in data or b'\n' in data:
        os.write(1, b'\r\n\x1b]133;C\x07ok\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ')
PY

# The relay: forwards both ways until a client sends the marker, then forwards
# that request and drops every later reply on every connection.
cat > "$D/relay.py" <<'PY'
import asyncio, os, sys
listen, upstream, marker, tripped_path = sys.argv[1], sys.argv[2], sys.argv[3].encode(), sys.argv[4]
tripped = False
async def pump(reader, writer, upward):
    global tripped
    tail = b""
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            if upward:
                window = tail + data
                if marker in window and not tripped:
                    tripped = True
                    writer.write(data)
                    await writer.drain()
                    with open(tripped_path, "w") as f:
                        f.write("tripped\n")
                    continue
                tail = window[-len(marker):]
            elif tripped:
                continue
            writer.write(data)
            await writer.drain()
    except (ConnectionError, OSError):
        pass
async def handle(cr, cw):
    try:
        ur, uw = await asyncio.open_unix_connection(upstream)
    except OSError:
        cw.close()
        return
    await asyncio.gather(pump(cr, uw, True), pump(ur, cw, False))
async def main():
    server = await asyncio.start_unix_server(handle, path=listen)
    async with server:
        await server.serve_forever()
asyncio.run(main())
PY

cleanup() {
    [[ -n "${WATCH:-}" ]] && kill "$WATCH" 2>/dev/null
    [[ -n "${RELAYPID:-}" ]] && kill "$RELAYPID" 2>/dev/null
    [[ -n "${MUXPID:-}" ]] && kill "$MUXPID" 2>/dev/null
    wait 2>/dev/null
    log "sandbox left at $D"
}
trap cleanup EXIT

env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" \
    -- /usr/bin/python3 "$D/pane.py" "$D/pane-input.log" > "$OUT/mux.log" 2>&1 &
MUXPID=$!
for _ in $(seq 1 150); do
    grep -q "pid=$MUXPID" "$SOCK.lock" 2>/dev/null && [[ -S "$SOCK" ]] && break
    sleep 0.2
done
[[ -S "$SOCK" ]] || { log "the mux did not start"; exit 2; }
PANE=$(ft list --json 2>/dev/null | /usr/bin/python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pane_id"])') \
    || { log "no pane"; exit 2; }
/usr/bin/python3 "$D/relay.py" "$RELAY" "$SOCK" "$MARKER" "$D/relay.tripped" > "$OUT/relay.log" 2>&1 &
RELAYPID=$!
env -i "${ENVV[@]}" "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK" "$FT_BIN" -c "$D/ft.toml" \
    watch --foreground --poll-interval 500 > "$OUT/watch.log" 2>&1 &
WATCH=$!
sleep 8
[[ -S "$RELAY" ]] || { log "the relay did not start"; exit 2; }

C=$D/tx-contract.json
/usr/bin/python3 - "$PANE" "$MARKER" > "$C" <<'PY'
import json, sys, time
pane, marker = int(sys.argv[1]), sys.argv[2]
tx = "e2e-reconcile"
print(json.dumps({
    "tx_version": 1,
    "intent": {"tx_id": tx, "requested_by": "operator", "summary": "reconcile after a crash",
               "correlation_id": tx, "created_at_ms": int(time.time() * 1000)},
    "plan": {"plan_id": f"plan-{tx}", "tx_id": tx,
             "steps": [{"step_id": "s1", "ordinal": 0,
                        "action": {"type": "send_text", "pane_id": pane, "text": marker},
                        "description": "type the marker at the prompt"}],
             "preconditions": [{"prompt_active": {"pane_id": pane}}],
             "compensations": []},
    "lifecycle_state": "planned", "outcome": "pending", "receipts": [],
}))
PY
sent() { grep -a -o "$MARKER" "$D/pane-input.log" 2>/dev/null | grep -c . ; }

log "pane $PANE, marker $MARKER: ft tx run through the relay"
env -i "${ENVV[@]}" "WEZTERM_UNIX_SOCKET=$RELAY" "FRANKENTERM_UNIX_SOCKET=$RELAY" "$FT_BIN" -c "$D/ft-relay.toml" \
    tx run --contract-file "$C" -f json > "$OUT/run1.json" 2> "$OUT/run1.err" &
FTPID=$!
for _ in $(seq 1 150); do [[ -f "$D/relay.tripped" ]] && break; kill -0 "$FTPID" 2>/dev/null || break; sleep 0.2; done
if [[ ! -f "$D/relay.tripped" ]]; then
    log "the send never reached the relay; ft exited first:"; tail -5 "$OUT/run1.err"; head -c 600 "$OUT/run1.json"; echo
    exit 2
fi
sleep 1
kill -0 "$FTPID" 2>/dev/null; check "ft is still waiting for the send's reply" $?
kill -KILL "$FTPID" 2>/dev/null; wait "$FTPID" 2>/dev/null
kill "$RELAYPID" 2>/dev/null; wait "$RELAYPID" 2>/dev/null; RELAYPID=
sleep 1
is "$(sent)" 1; check "after the crash the pane has the text once" $? "($(sent))"

ft tx show --contract-file "$C" --include-contract -f json > "$OUT/show1.json" 2> "$OUT/show1.err"
ft tx run --contract-file "$C" -f json > "$OUT/run2.json" 2> "$OUT/run2.err"; RC=$?
json_is "$OUT/run2.json" 'd.get("ok") is False and d.get("error_code") == "mission.tx.execution_failed" and "prior outcome Pending" in d.get("error", "")'
check "ft tx run refuses the Pending step" $? "(exit $RC: $(head -c 300 "$OUT/run2.json"))"
json_is "$OUT/run2.json" '"ft tx reconcile --step s1 " in (d.get("hint") or "")'
check "its hint names ft tx reconcile for that step" $? "(hint: $(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("hint"))' "$OUT/run2.json" 2>/dev/null))"
ft tx rollback --contract-file "$C" -f json > "$OUT/rollback.json" 2> "$OUT/rollback.err"; RC=$?
json_is "$OUT/rollback.json" 'd.get("ok") is False and "ft tx reconcile --step" in (d.get("hint") or "")'
check "ft tx rollback refuses it and its hint points at ft tx reconcile" $? "(exit $RC: $(head -c 400 "$OUT/rollback.json"))"
is "$(sent)" 1; check "the refused run and rollback sent nothing" $? "($(sent))"

EVID="pane-input.log shows the marker once; the send reached the pane"
ft tx reconcile --contract-file "$C" --step s1 --outcome success --evidence "$EVID" -f json \
    > "$OUT/rec1.json" 2> "$OUT/rec1.err"; RC=$?
CODE=$(/usr/bin/python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); d=d.get("data", d)
print(d.get("allow_once_code") or "") if d.get("status") == "approval_required" else print("")' "$OUT/rec1.json" 2>/dev/null)
is "$RC:${CODE:+code}" "0:code"
check "ft tx reconcile without a code prints an allow-once code" $? "(exit $RC: $(head -c 300 "$OUT/rec1.json") $(tail -2 "$OUT/rec1.err"))"

ft tx reconcile --contract-file "$C" --step s1 --outcome success --evidence "$EVID" --approval-code WRONG1 -f json \
    > "$OUT/rec-wrong.json" 2> "$OUT/rec-wrong.err"; RC=$?
json_is "$OUT/rec-wrong.json" 'd.get("ok") is False and d.get("error_code") == "mission.tx.reconcile_not_proven"'
check "a wrong code is refused" $? "(exit $RC: $(head -c 300 "$OUT/rec-wrong.json"))"

ft tx reconcile --contract-file "$C" --step s1 --outcome success --evidence "$EVID" --approval-code "$CODE" -f json \
    > "$OUT/rec2.json" 2> "$OUT/rec2.err"; RC=$?
/usr/bin/python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); d=d.get("data", d)
sys.exit(0 if d.get("status") == "reconciled" and d.get("outcome") == "success" and d.get("receipt") else 1)' "$OUT/rec2.json" 2>/dev/null
check "with the code it settles the step as success with a receipt" $? "(exit $RC: $(head -c 400 "$OUT/rec2.json") $(tail -2 "$OUT/rec2.err"))"

ft tx reconcile --contract-file "$C" --step s1 --outcome success --evidence "$EVID" --approval-code "$CODE" -f json \
    > "$OUT/rec3.json" 2> "$OUT/rec3.err"; RC=$?
json_is "$OUT/rec3.json" 'd.get("ok") is False and str(d.get("error_code", "")).startswith("mission.tx.reconcile_")'
check "the same code a second time is refused" $? "(exit $RC: $(head -c 300 "$OUT/rec3.json"))"

ft tx run --contract-file "$C" -f json > "$OUT/run3.json" 2> "$OUT/run3.err"; RC=$?
FINAL=$(/usr/bin/python3 -c 'import json,sys
d=json.load(open(sys.argv[1])); print((d.get("data") or {}).get("final_state", ""))' "$OUT/run3.json" 2>/dev/null)
is "$RC:$FINAL" "0:committed"
check "ft tx run then completes the transaction" $? "(exit $RC, final_state=$FINAL: $(head -c 400 "$OUT/run3.json") $(tail -2 "$OUT/run3.err"))"
sleep 1
is "$(sent)" 1; check "the pane still has the text once" $? "($(sent))"
ft tx show --contract-file "$C" --include-contract -f json > "$OUT/show2.json" 2> "$OUT/show2.err"

log "$PASS passed, $FAIL failed (artifacts: $OUT)"
[[ "$FAIL" == 0 ]]
