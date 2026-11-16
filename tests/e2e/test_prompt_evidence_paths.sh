#!/usr/bin/env bash
# test_prompt_evidence_paths.sh — E2E: prompt evidence decides untrusted sends (ft-xxfwy.13)
#
# Each scenario runs a private frankenterm-mux-server whose only pane is either
#   integrated: a fixture shell that emits OSC 133 prompt/command markers, or
#   bare:       `zsh -f` with no shell integration,
# then drives `ft robot send` with and without a watcher.
#
# Expected:
#   integrated + watcher -> ok, injection allowed, capabilities.prompt_active=true,
#                           doctor "prompt evidence" ok
#   bare + watcher       -> requires_approval, rule policy.prompt_unknown, reason names
#                           `ft setup shell`, doctor "prompt evidence" warning
#   either, no watcher   -> robot.approval_error naming `ft watch` (never a raw
#                           FOREIGN KEY failure)
#   fullscreen + watcher -> denied, policy.alt_screen, capabilities.alt_screen=true
#                           (alt-screen state comes from the direct-mux listing)
#   `ft tx run` with a prompt_active precondition commits on the integrated pane
#   and is denied at prepare (typed prompt_active.inactive reason) on the bare one.
#
# Needs native `ft` and `frankenterm-mux-server` binaries: set FT_BIN_DIR (default
# target/debug). Envelopes, doctor output and watcher/mux logs are retained under
# tests/e2e/artifacts/prompt-evidence/<run>/.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
BIN="${FT_BIN_DIR:-${REPO_ROOT}/target/debug}"
FT_BIN="${BIN}/ft"
MUX_BIN="${BIN}/frankenterm-mux-server"
RUN_ID="$(date +%Y%m%dT%H%M%S)"
OUT="${SCRIPT_DIR}/artifacts/prompt-evidence/${RUN_ID}"
PASS=0
FAIL=0

for bin in "$FT_BIN" "$MUX_BIN"; do
    if [[ ! -x "$bin" ]]; then
        echo "SKIP: $bin not built (set FT_BIN_DIR)" >&2
        exit 2
    fi
done
mkdir -p "$OUT"

check() { # name  condition-exit-status  detail
    if [[ "$2" == 0 ]]; then
        PASS=$((PASS + 1)); echo "PASS $1"
    else
        FAIL=$((FAIL + 1)); echo "FAIL $1: $3"
    fi
    printf '{"check":"%s","ok":%s}\n' "$1" "$([[ "$2" == 0 ]] && echo true || echo false)" >> "$OUT/checks.jsonl"
}

# json_expr FILE PYTHON_EXPR — evaluate EXPR against the parsed JSON as `d`.
json_expr() {
    python3 - "$1" "$2" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as exc:
    print(f"unparseable: {exc}"); sys.exit(1)
inj = (d.get("data") or {}).get("injection") or {}
dec = inj.get("decision") or {}
caps = (dec.get("context") or {}).get("capabilities") or {}
doctor = {c.get("name"): c for c in d.get("checks", [])} if isinstance(d, dict) else {}
ok = eval(sys.argv[2])
if not ok:
    print(json.dumps(d)[:600])
sys.exit(0 if ok else 1)
PY
}

cat > "$OUT/osc133-pane.py" <<'PY'
import os, select, sys, tty
tty.setraw(sys.stdin.fileno())
os.write(1, b'\x1b]133;A\x07$ ')
while True:
    r, _, _ = select.select([0], [], [], 120)
    if not r:
        break
    data = os.read(0, 4096)
    if not data:
        break
    if b'\r' in data or b'\n' in data:
        os.write(1, b'\r\n\x1b]133;C\x07ok\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ')
PY

cat > "$OUT/fullscreen-pane.py" <<'PY'
import os, select, sys, tty
tty.setraw(sys.stdin.fileno())
os.write(1, b'\x1b]133;A\x07$ ')
os.write(1, b'\x1b[?1049h\x1b[HFULLSCREEN APP')
while True:
    r, _, _ = select.select([0], [], [], 120)
    if not r:
        break
    if not os.read(0, 4096):
        break
PY

scenario() { # name  pane-command...
    local name="$1"; shift
    local D; D=$(mktemp -d /tmp/ftpe-XXXXXX)
    mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
    chmod 700 "$D" "$D/.ft" "$D/runtime"
    local SOCK="$D/mux.sock"
    printf '[[unix_domains]]\nname = "pe"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
    printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
    local ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
        "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
        "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
        "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
        "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
        "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
    env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- "$@" \
        > "$OUT/$name.mux.log" 2>&1 &
    local MUXPID=$!
    for _ in $(seq 1 150); do
        grep -q "pid=$MUXPID" "$SOCK.lock" 2>/dev/null && [[ -S "$SOCK" ]] && break
        sleep 0.2
    done
    ft() { env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" "$@"; }
    local PANE
    PANE=$(ft list --json 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pane_id"])')
    sleep 2

    ft robot send "$PANE" "echo ok" --no-paste > "$OUT/$name.nowatch.json" 2> "$OUT/$name.nowatch.err"
    local detail
    detail=$(json_expr "$OUT/$name.nowatch.json" \
        'd.get("ok") is False and d.get("error_code") == "robot.approval_error" and "ft watch" in d.get("error","") and "FOREIGN KEY" not in d.get("error","")')
    check "$name/no-watcher names ft watch" $? "$detail"

    env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" watch --foreground --poll-interval 500 \
        > "$OUT/$name.watch.log" 2>&1 &
    local WATCH=$!
    sleep 8
    ft send "$PANE" "echo warm" --no-paste > "$OUT/$name.human.txt" 2>&1
    sleep 4
    ft robot send "$PANE" "echo ok" --no-paste > "$OUT/$name.watch.json" 2> "$OUT/$name.watch.err"
    ft doctor --json > "$OUT/$name.doctor.json" 2> /dev/null
    # Contracts must live inside the workspace root.
    python3 - "$PANE" "$name" > "$D/tx-contract.json" <<'PY'
import json, sys, time
pane, name = int(sys.argv[1]), sys.argv[2]
tx = f"e2e-prompt-{name}"
print(json.dumps({
    "tx_version": 1,
    "intent": {"tx_id": tx, "requested_by": "operator", "summary": "prompt_active gate",
               "correlation_id": tx, "created_at_ms": int(time.time() * 1000)},
    "plan": {"plan_id": f"plan-{tx}", "tx_id": tx,
             "steps": [{"step_id": "s1", "ordinal": 0,
                        "action": {"type": "send_text", "pane_id": pane, "text": "echo tx"},
                        "description": "send at the prompt"}],
             "preconditions": [{"prompt_active": {"pane_id": pane}}],
             "compensations": []},
    "lifecycle_state": "planned", "outcome": "pending", "receipts": [],
}))
PY
    ft tx run --contract-file "$D/tx-contract.json" --format json \
        > "$OUT/$name.tx-run.json" 2> "$OUT/$name.tx-run.err"
    echo "tx exit=$?" >> "$OUT/$name.tx-run.err"
    cp "$D/tx-contract.json" "$OUT/$name.tx-contract.json"

    if [[ "$name" == fullscreen ]]; then
        detail=$(json_expr "$OUT/$name.watch.json" \
            'inj.get("status") == "denied" and dec.get("rule_id") == "policy.alt_screen" and caps.get("alt_screen") is True')
        check "$name/watcher send into the alternate screen is denied" $? "$detail"
    elif [[ "$name" == integrated ]]; then
        detail=$(json_expr "$OUT/$name.watch.json" \
            'd.get("ok") is True and inj.get("status") == "allowed" and caps.get("prompt_active") is True')
        check "$name/watcher send allowed on prompt evidence" $? "$detail"
        detail=$(json_expr "$OUT/$name.doctor.json" 'doctor.get("prompt evidence", {}).get("status") == "ok"')
        check "$name/doctor prompt evidence ok" $? "$detail"
        detail=$(json_expr "$OUT/$name.tx-run.json" \
            '(d.get("data") or {}).get("final_state") == "committed" and d["data"]["prepare_report"]["gate_inputs"][0]["preconditions_satisfied"] is True')
        check "$name/tx prompt_active precondition passes and commits" $? "$detail"
    else
        detail=$(json_expr "$OUT/$name.watch.json" \
            'inj.get("status") == "requires_approval" and dec.get("rule_id") == "policy.prompt_unknown" and "ft setup shell" in dec.get("reason","")')
        check "$name/watcher send needs approval naming ft setup shell" $? "$detail"
        detail=$(json_expr "$OUT/$name.doctor.json" 'doctor.get("prompt evidence", {}).get("status") == "warning"')
        check "$name/doctor prompt evidence warns" $? "$detail"
        detail=$(json_expr "$OUT/$name.tx-run.json" \
            '(d.get("data") or {}).get("final_state") == "failed" and d["data"]["prepare_report"]["outcome"] == "denied" and d["data"]["prepare_report"]["gate_inputs"][0].get("precondition_reason_code", "").startswith("tx.prepare.precondition.prompt_active.inactive") and "commit_report" not in d["data"]')
        check "$name/tx prompt_active precondition fails closed at prepare" $? "$detail"
    fi

    kill "$WATCH" 2> /dev/null; wait "$WATCH" 2> /dev/null
    kill "$MUXPID" 2> /dev/null; wait "$MUXPID" 2> /dev/null
    cp -R "$D/.ft" "$OUT/$name.ft" 2> /dev/null
}

scenario integrated /usr/bin/python3 "$OUT/osc133-pane.py"
scenario bare /bin/zsh -f
scenario fullscreen /usr/bin/python3 "$OUT/fullscreen-pane.py"

echo "prompt evidence e2e: $PASS passed, $FAIL failed (artifacts: $OUT)"
[[ "$FAIL" == 0 ]]
