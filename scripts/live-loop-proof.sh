#!/usr/bin/env bash
# live-loop-proof.sh — tier 2 live-loop proof on a private mux (ft-xxfwy.10)
#
# 20 live panes on a real frankenterm-mux-server (vendored streaming capture), spawned
# through `ft robot profile apply`. Agent panes are SCRIPTED fixtures (title + real
# detection text + OSC 133 prompt), not real agents: no model is called. Every check is
# authoritative: the run fails if any predicate fails.
#
#   1. >= 20 panes live, spawned by profile apply, each actually running its command
#   2. detections from >= 3 rule families: codex.usage.reached, claude_code.compaction,
#      gemini.usage.reached
#   3. ft watch --auto-handle runs handle_compaction and its prompt reaches a claude pane
#   4. a robot send into a full-screen pane is denied (policy.alt_screen) and audited
#   5. FTS search finds the usage-limit text in >= 2 panes
#   6. `ft robot state` latency p95 < 500 ms (asserted on every build profile)
#
# LIVE_LOOP_TIER=3 runs tier 3 (ft-xxfwy.10): 50 panes (40 agent fixtures, 2 full-screen,
# 10 shells that flood 150k lines), a small [fleet_scrollback] per_pane_budget_bytes, agent
# fires staggered over ~4 minutes, and four more checks:
#   7. the watcher's fleet pressure tier rises above Normal (sampled from `ft robot health`)
#   8. hot->warm scrollback spill is observed (mux warm_spill_lines_total > 0)
#   9. detection latency p95 < 5 s (send of "fire" -> event captured_at, first event per pane)
#  10. no capture gaps except explicit resets (alt-screen transitions); the receipt also
#      records memory attribution from the mux (doctor "mux scrollback", fleet telemetry)
#
# usage: [LIVE_LOOP_TIER=3] [LIVE_LOOP_RETAIN=1] scripts/live-loop-proof.sh [BIN_DIR]
# Writes tests/e2e/artifacts/live-loop/<run>/receipt.json plus all logs.
set -uo pipefail
umask 077

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$(cd "${1:-$REPO_ROOT/target/debug}" && pwd)"
FT_BIN="$BIN/ft"; MUX_BIN="$BIN/frankenterm-mux-server"
for bin in "$FT_BIN" "$MUX_BIN"; do [[ -x "$bin" ]] || { echo "missing $bin" >&2; exit 2; }; done
OUT="$REPO_ROOT/tests/e2e/artifacts/live-loop/$(date +%Y%m%dT%H%M%S)"
mkdir -p "$OUT"
# Cargo names the output directory after the profile (debug, release-perf,
# release-interactive, ...); a measurement applies only to the profile it ran on.
PROFILE=$(basename "$BIN")
TIER=${LIVE_LOOP_TIER:-2}
if [[ "$TIER" == 3 ]]; then
  SPECS=${LIVE_LOOP_SPECS:-codex:14 claude:14 gemini:10 fullscreen:2}
  SHELLS=${LIVE_LOOP_SHELLS:-10}
  MIN_PANES=50
  # Shells flood past the hot tier so warm spill (and budget pressure) is real.
  SHELL_CMD="/bin/sh -c 'seq 1 150000; exec /bin/zsh -f'"
  FIRE_STAGGER=6
else
  SPECS=${LIVE_LOOP_SPECS:-codex:6 claude:6 gemini:4 fullscreen:1}
  SHELLS=${LIVE_LOOP_SHELLS:-3}
  MIN_PANES=20
  SHELL_CMD="/bin/zsh -f"
  FIRE_STAGGER=0
fi

D=$(mktemp -d /tmp/ftll-XXXXXX)
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp" "$D/inputs"
chmod 700 "$D" "$D/.ft" "$D/runtime"
SOCK="$D/mux.sock"
printf '[[unix_domains]]\nname = "ll"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
cat > "$D/ft.toml" <<EOF
[storage]
db_path = "ft.db"
[vendored]
mux_socket_path = "$SOCK"
[workflows]
enabled = ["handle_compaction"]
auto_run_allowlist = ["handle_compaction"]
max_concurrent = 2
[workflows.compaction_prompts.by_agent]
claude_code = "TIER2_CONTEXT_REFRESH\n"
EOF
if [[ "$TIER" == 3 ]]; then
  # A real operator budget, sized so flooding panes exceed it.
  printf '[fleet_scrollback]\nenabled = true\nper_pane_budget_bytes = 262144\n' >> "$D/ft.toml"
fi
chmod 600 "$D/ft.toml"

cat > "$D/agent-pane.py" <<'PY'
# Scripted agent pane: title, OSC 133 prompt, real detection text on "fire".
import os, pathlib, select, sys, termios, time, tty
kind, inputs = sys.argv[1], pathlib.Path(sys.argv[2])
# TCSANOW: the default TCSAFLUSH discards input typed before startup finished.
tty.setraw(sys.stdin.fileno(), termios.TCSANOW)
lines = {
    "codex": b"You've reached your usage limit. try again at 3:00 PM.",
    "claude": b"Conversation compacted: 91234 tokens to 4512",
    "gemini": b"Usage limit reached for all Pro models.",
}
title = {"codex": b"codex", "claude": b"claude", "gemini": b"gemini", "fullscreen": b"htop"}[kind]
os.write(1, b"\x1b]2;" + title + b"\x07")
if kind == "fullscreen":
    os.write(1, b"\x1b[?1049h\x1b[HFULLSCREEN")
else:
    os.write(1, b"\x1b]133;A\x07" + title + b"> ")
record = inputs / f"{os.getpid()}.{kind}.in"
(inputs / f"{os.getpid()}.{kind}.ready").write_text(f"{time.time():.3f}\n")
deadline = time.monotonic() + 600
while time.monotonic() < deadline:
    ready, _, _ = select.select([0], [], [], 1)
    if not ready:
        # Real agent TUIs keep re-publishing their title; a one-shot title at
        # startup can be lost while the pane is still being attached.
        os.write(1, b"\x1b]2;" + title + b"\x07")
        continue
    data = os.read(0, 4096)
    if not data:
        break
    with record.open("ab") as handle:
        handle.write(data)
    if kind != "fullscreen" and (b"\r" in data or b"\n" in data):
        text = lines.get(kind, b"ok") if b"fire" in data else b"ok"
        os.write(1, b"\r\n\x1b]133;C\x07" + text + b"\r\n\x1b]133;D;0\x07\x1b]133;A\x07" + title + b"> ")
PY

ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home" "SHELL=/bin/zsh"
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
  cp -R "$D/inputs" "$OUT/pane-inputs" 2> /dev/null
}
trap cleanup EXIT

CHECKS=()
check() { # name exit-status detail
  local ok=false; [[ "$2" == 0 ]] && ok=true
  echo "$([[ $ok == true ]] && echo PASS || echo FAIL) $1 ${3:+- $3}"
  CHECKS+=("$(python3 -c 'import json,sys;print(json.dumps({"name":sys.argv[1],"ok":sys.argv[2]=="true","detail":sys.argv[3]}))' "$1" "$ok" "${3:-}")")
}

env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- /bin/zsh -f \
  > "$OUT/mux.log" 2>&1 &
MUXPID=$!; PIDS+=("$MUXPID")
for _ in $(seq 1 150); do grep -q "pid=$MUXPID" "$SOCK.lock" 2> /dev/null && [[ -S "$SOCK" ]] && break; sleep 0.2; done

for spec in $SPECS; do
  kind=${spec%%:*}; count=${spec##*:}
  ft robot profile create "${kind}_ws" --command "/usr/bin/python3 $D/agent-pane.py $kind $D/inputs" \
    > "$OUT/profile-create-$kind.json" 2>&1
  ft robot profile apply "${kind}_ws" --count "$count" > "$OUT/profile-apply-$kind.json" 2>&1
done
ft robot profile create shell_ws --command "$SHELL_CMD" > "$OUT/profile-create-shell.json" 2>&1
ft robot profile apply shell_ws --count "$SHELLS" > "$OUT/profile-apply-shell.json" 2>&1

env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" watch --foreground --auto-handle --poll-interval 500 \
  > "$OUT/watch.log" 2>&1 &
PIDS+=("$!")
# Wait until every scripted pane's fixture is running before driving it.
FIXTURES=0
for spec in $SPECS; do FIXTURES=$((FIXTURES + ${spec##*:})); done
for _ in $(seq 1 60); do
  READY=$(find "$D/inputs" -name '*.ready' | wc -l | tr -d ' ')
  (( READY >= FIXTURES )) && break
  sleep 0.5
done
check "every_profile_pane_runs_its_command" $(( READY >= FIXTURES ? 0 : 1 )) "$READY/$FIXTURES fixtures started"
sleep 8

ft robot --format json state > "$OUT/state.json" 2> /dev/null
python3 - "$OUT/state.json" "$OUT/panes.tsv" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
panes = data.get("data", data)
panes = panes.get("panes", panes) if isinstance(panes, dict) else panes
with open(sys.argv[2], "w") as out:
    for pane in panes:
        out.write(f"{pane['pane_id']}\t{(pane.get('title') or '').strip()}\n")
PY
PANES=$(wc -l < "$OUT/panes.tsv" | tr -d ' ')
(( PANES >= MIN_PANES ))
check "at_least_${MIN_PANES}_live_panes_via_profile_apply" $? "$PANES panes"

# Tier 3: sample the watcher's health (fleet tier, warm spill) until told to stop.
if [[ "$TIER" == 3 ]]; then
  (
    while [[ ! -e "$OUT/health-stop" ]]; do
      ft robot --format json health 2> /dev/null | python3 -c '
import json, sys, time
t = sys.stdin.read()
try:
    h = json.loads(t[t.index("{"):])["data"]["health"]
except Exception:
    sys.exit(0)
tel = h.get("fleet_scrollback_telemetry") or {}
print(json.dumps({"t": time.time(), "fleet_pressure_tier": h.get("fleet_pressure_tier"),
                  "warm_spill_lines_total": tel.get("warm_spill_lines_total"),
                  "observed_panes": h.get("observed_panes")}))' >> "$OUT/health-samples.jsonl"
      sleep 15
    done
  ) &
  PIDS+=("$!")
fi

# Pane ids come from each profile apply receipt (titles lag in pane metadata).
spawned() {
  python3 -c 'import json,sys;t=open(sys.argv[1]).read();d=json.loads(t[t.index("{"):]);print(" ".join(str(p) for p in d["data"]["panes_spawned"]))' \
    "$OUT/profile-apply-$1.json" 2> /dev/null
}
# Fire every agent pane once (staggered across the run in tier 3), recording when each
# send returned so detection latency can be measured against the event's captured_at.
: > "$OUT/fire-times.tsv"
for kind in codex claude gemini; do
  for pane in $(spawned "$kind"); do
    ft send --no-paste "$pane" "fire" > /dev/null 2>&1
    printf '%s\t%s\n' "$pane" "$(python3 -c 'import time;print(int(time.time()*1000))')" >> "$OUT/fire-times.tsv"
    (( FIRE_STAGGER > 0 )) && sleep "$FIRE_STAGGER"
  done
done
sleep 15

ft robot --format json events --limit 1000 > "$OUT/events.json" 2> /dev/null
for rule in codex.usage.reached claude_code.compaction gemini.usage.reached; do
  python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));ev=d["data"]["events"];sys.exit(0 if any(e["rule_id"]==sys.argv[2] for e in ev) else 1)' \
    "$OUT/events.json" "$rule"
  check "detects_$rule" $? ""
done

sleep 10
grep -l "TIER2_CONTEXT_REFRESH" "$D"/inputs/*.claude.in > "$OUT/workflow-delivered.txt" 2> /dev/null
[[ -s "$OUT/workflow-delivered.txt" ]]
check "auto_handle_compaction_prompt_reaches_a_claude_pane" $? "$(wc -l < "$OUT/workflow-delivered.txt" | tr -d ' ') pane(s)"

for kind in codex claude gemini fullscreen shell; do
  for pane in $(spawned "$kind"); do ft get-text "$pane" --tail 60 > "$OUT/screen-$kind-$pane.txt" 2>&1; done
done
DENIED=0; AUDITED=0; FULL_PANES=$(spawned fullscreen)
for FULL in $FULL_PANES; do
  ft robot send "$FULL" "x" --no-paste > "$OUT/fullscreen-send-$FULL.json" 2> /dev/null
  python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));inj=(d.get("data") or {}).get("injection") or {};dec=inj.get("decision") or {};sys.exit(0 if inj.get("status")=="denied" and dec.get("rule_id")=="policy.alt_screen" else 1)' \
    "$OUT/fullscreen-send-$FULL.json" && DENIED=$((DENIED + 1))
  ft audit --format json --pane "$FULL" --limit 50 > "$OUT/fullscreen-audit-$FULL.json" 2> /dev/null
  python3 -c 'import json,sys;rows=json.load(open(sys.argv[1]));sys.exit(0 if any(r.get("rule_id")=="policy.alt_screen" and r.get("policy_decision")=="deny" for r in rows) else 1)' \
    "$OUT/fullscreen-audit-$FULL.json" && AUDITED=$((AUDITED + 1))
done
FULL_COUNT=$(echo $FULL_PANES | wc -w | tr -d ' ')
(( FULL_COUNT > 0 && DENIED == FULL_COUNT ))
check "robot_send_into_fullscreen_pane_is_denied" $? "$DENIED/$FULL_COUNT full-screen panes denied"
(( FULL_COUNT > 0 && AUDITED == FULL_COUNT ))
check "alt_screen_denial_has_audit_row" $? "$AUDITED/$FULL_COUNT audited"

ft robot --format json search "usage limit" --limit 50 > "$OUT/search.json" 2> /dev/null
HIT_PANES=$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));r=d["data"].get("results") or d["data"].get("hits") or [];print(len({h["pane_id"] for h in r}))' "$OUT/search.json" 2> /dev/null || echo 0)
(( HIT_PANES >= 2 ))
check "fts_search_finds_usage_limit_across_panes" $? "$HIT_PANES panes"

if [[ "$TIER" == 3 ]]; then
  # The fleet tier needs 3 sustained evaluations on the watcher's 60 s maintenance
  # loop; keep sampling until it moves or the budget runs out.
  for _ in $(seq 1 16); do
    grep -q '"fleet_pressure_tier": "\(Elevated\|Critical\|Emergency\)"' "$OUT/health-samples.jsonl" 2> /dev/null && break
    sleep 15
  done
  touch "$OUT/health-stop"
  TIERS=$(python3 -c 'import json,sys;print(",".join(sorted({json.loads(l).get("fleet_pressure_tier") or "none" for l in open(sys.argv[1])})))' \
    "$OUT/health-samples.jsonl" 2> /dev/null)
  grep -q '"fleet_pressure_tier": "\(Elevated\|Critical\|Emergency\)"' "$OUT/health-samples.jsonl" 2> /dev/null
  check "fleet_pressure_tier_rises_above_normal" $? "tiers seen: ${TIERS:-none}"
  SPILL=$(python3 -c 'import json,sys;print(max((json.loads(l).get("warm_spill_lines_total") or 0) for l in open(sys.argv[1])))' \
    "$OUT/health-samples.jsonl" 2> /dev/null || echo 0)
  (( SPILL > 0 ))
  check "hot_to_warm_scrollback_spill_observed" $? "${SPILL} lines spilled to warm"
  python3 - "$OUT/fire-times.tsv" "$OUT/events.json" "$OUT/detection-latency.txt" << 'PY'
import json, sys
fired = {int(p): int(t) for p, t in (line.split("\t") for line in open(sys.argv[1]) if line.strip())}
events = json.load(open(sys.argv[2]))["data"]["events"]
agent_rules = ("codex.usage.reached", "claude_code.compaction", "gemini.usage.reached")
first = {}
for e in events:
    pane, at = e.get("pane_id"), e.get("captured_at")
    if pane in fired and e.get("rule_id") in agent_rules and at is not None and at >= fired[pane] - 1000:
        first[pane] = min(first.get(pane, at), at)
lat = sorted(max(0, first[p] - fired[p]) for p in first)
p95 = lat[max(0, int(len(lat) * 0.95) - 1)] if lat else -1
open(sys.argv[3], "w").write(f"{p95} {len(lat)} {len(fired)}\n")
PY
  read -r DET_P95 DET_N DET_FIRED < "$OUT/detection-latency.txt"
  (( DET_P95 >= 0 && DET_P95 < 5000 && DET_N == DET_FIRED ))
  check "detection_latency_p95_under_5s" $? "p95 ${DET_P95} ms over ${DET_N}/${DET_FIRED} fired panes"
  # Capture gaps: only explicit resets (alt-screen transitions) are acceptable.
  sqlite3 -readonly "$D/.ft/ft.db" "select reason, count(*) from output_gaps group by reason" \
    > "$OUT/gaps.txt" 2> "$OUT/gaps.err"
  GAPS_OK=$?
  UNEXPLAINED=$(grep -v -E '^alt_screen_(entered|exited|toggled)\|' "$OUT/gaps.txt" | tr '\n' ' ')
  (( GAPS_OK == 0 )) && [[ -z "$UNEXPLAINED" ]]
  check "no_capture_gaps_except_explicit_resets" $? "${UNEXPLAINED:-$(tr '\n' ' ' < "$OUT/gaps.txt")}"
  # Memory attribution, from the mux's own accounting (the resource cockpit's
  # pane_budget domain reports no telemetry): retained in the receipt.
  ft doctor --json > "$OUT/doctor-final.json" 2> /dev/null
  python3 - "$OUT/doctor-final.json" "$OUT/health-samples.jsonl" "$OUT/memory.json" << 'PY'
import json, sys
t = open(sys.argv[1]).read()
checks = json.loads(t[t.index("{"):]).get("checks", [])
row = next((c for c in checks if c.get("name") == "mux scrollback"), {})
samples = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
json.dump({"mux_scrollback": row.get("detail"), "last_health_sample": samples[-1] if samples else None},
          open(sys.argv[3], "w"))
PY
fi

python3 - "$FT_BIN" "$D/ft.toml" "$OUT/state-latency.txt" "${ENVV[@]}" <<'PY'
import subprocess, sys, time
ft, config, out, env = sys.argv[1], sys.argv[2], sys.argv[3], dict(e.split("=", 1) for e in sys.argv[4:])
samples = []
for _ in range(20):
    start = time.perf_counter()
    subprocess.run([ft, "-c", config, "robot", "--format", "json", "state"], env=env, capture_output=True)
    samples.append((time.perf_counter() - start) * 1000)
samples.sort()
open(out, "w").write(f"{samples[int(len(samples) * 0.95) - 1]:.0f}\n")
PY
P95=$(cat "$OUT/state-latency.txt")
(( P95 < 500 )); check "robot_state_p95_under_500ms" $? "${P95} ms ($PROFILE build)"

python3 - "$OUT/receipt.json" "$PROFILE" "$PANES" "$P95" "$("$FT_BIN" --version 2> /dev/null)" "$TIER" "${CHECKS[@]}" <<'PY'
import json, platform, subprocess, sys, time
checks = [json.loads(c) for c in sys.argv[7:]]
receipt = {
    "schema": "ft.live-loop-proof.v1", "tier": int(sys.argv[6]), "adapter": "live-mux",
    "panes": "scripted (no model calls)", "build_profile": sys.argv[2],
    "pane_count": int(sys.argv[3]), "robot_state_p95_ms": int(sys.argv[4]),
    "host": platform.node(), "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "commit": subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
    "crates_dirty": bool(subprocess.run(["git", "status", "--short", "--", "crates"], capture_output=True, text=True).stdout.strip()),
    "ft_version": sys.argv[5].strip(),
    "status": "pass" if checks and all(c["ok"] for c in checks) else "fail",
    "checks": checks,
}
import os
memory = os.path.join(os.path.dirname(sys.argv[1]), "memory.json")
if os.path.exists(memory):
    receipt["memory_attribution"] = json.load(open(memory))
json.dump(receipt, open(sys.argv[1], "w"), indent=2)
print(f"live-loop tier {receipt['tier']}: {receipt['status']} ({sum(c['ok'] for c in checks)}/{len(checks)})")
PY
python3 -c 'import json,sys;sys.exit(0 if json.load(open(sys.argv[1]))["status"]=="pass" else 1)' "$OUT/receipt.json" || exit 1
# LIVE_LOOP_RETAIN=1 keeps a passing receipt as the tier's attestation.
if [[ "${LIVE_LOOP_RETAIN:-0}" == 1 ]]; then
  cp "$OUT/receipt.json" "$REPO_ROOT/docs/attestations/proofs/live-loop-tier$TIER.json"
fi
