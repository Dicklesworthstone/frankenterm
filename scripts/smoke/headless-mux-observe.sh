#!/usr/bin/env bash
# Headless observe smoke: the real vendored `frankenterm-mux-server` plus `ft`
# from the SAME build, no GUI. Proves discover -> attach -> list -> send ->
# observe -> detect on one machine and writes a JSON receipt plus a log.
#
# This is a release gate only when DSR invokes it on the native macOS binaries
# from the exact release build. A standalone invocation remains a dev signal:
# it runs whatever binaries you point it at. It exists because the first run
# (2026-09-02) found that every client connect aborted the mux server (fixed in
# 647d87fd6), so run it after touching mux-server-impl/local.rs,
# promise/spawn.rs, the vendored streaming client, or the pattern engine.
#
# Usage:
#   scripts/smoke/headless-mux-observe.sh [BIN_DIR] [OUT_DIR]
#   BIN_DIR defaults to target/debug; needs `ft` and `frankenterm-mux-server`.
#   OUT_DIR defaults to a fresh mktemp dir; receives receipt.json and smoke.log.
#
# Receipt schema (ft.smoke.headless-mux-observe.v1): generated_at, host, commit,
# codec_version, cli_version, mux_version, bin_dir, status (pass|fail), steps[] of
# {name, status (pass|fail), detail}. A receipt is `pass` only when every step
# passed; there is no skipped state.
#
# FT_SMOKE_KILL_SWITCH=1 additionally exercises one long-lived watcher's real
# compaction workflow against an owned PTY input recorder, before/after a trip
# from the real CLI and after reset. This never launches a GUI or a real agent.
# Every process gets a private workspace, config, home and socket. All evidence
# is retained. FT_SMOKE_SOURCE_SHA binds an RCH invocation to its retained build
# transcript; without it the receipt is explicitly only a development signal.
set -u
umask 077

BIN_DIR="${1:-target/debug}"
BIN_DIR=$(cd "$BIN_DIR" && pwd -P) || exit 2
FT="$BIN_DIR/ft"
MUX="$BIN_DIR/frankenterm-mux-server"
for bin in "$FT" "$MUX"; do
  [ -x "$bin" ] || { echo "missing binary: $bin" >&2; exit 2; }
done

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd -P) || exit 2
RELEASE_COMMIT="${FT_SMOKE_SOURCE_SHA:-}"
SOURCE_AUTHORITY=retained_remote_build_transcript
if [ -z "$RELEASE_COMMIT" ]; then
  SOURCE_AUTHORITY=development_checkout_only
  RELEASE_COMMIT=$(git -C "$REPO_ROOT" rev-parse HEAD) || {
    echo "cannot bind smoke receipt to a release commit" >&2
    exit 2
  }
fi
[[ "$RELEASE_COMMIT" =~ ^[0-9a-f]{40}$ ]] || { echo "source SHA must contain 40 lowercase hex digits" >&2; exit 2; }
CODEC_VERSION=$(sed -nE \
  's/^pub const CODEC_VERSION: usize = ([0-9]+);$/\1/p' \
  "$REPO_ROOT/frankenterm/codec/src/lib.rs")
case "$CODEC_VERSION" in
  ''|*[!0-9]*) echo "cannot bind smoke receipt to one codec version" >&2; exit 2 ;;
esac

D="${2:-$(mktemp -d "${TMPDIR:-/tmp}/ft-smoke-XXXXXX")}"
if [ -d "$D" ] && [ -n "$(ls -A "$D")" ]; then
  echo "refusing to overwrite a nonempty evidence directory: $D" >&2
  exit 2
fi
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
D=$(cd "$D" && pwd -P) || exit 2
chmod 700 "$D" "$D/.ft" "$D/runtime"
SOCK="$D/mux.sock"
[ "${#SOCK}" -le 90 ] || { echo "private socket path too long; choose a shorter evidence directory" >&2; exit 2; }
KILL_SWITCH_SMOKE="${FT_SMOKE_KILL_SWITCH:-0}"
PYTHON=$(command -v python3) || exit 2
case "$PYTHON" in /*) ;; *) echo "python3 must resolve to an absolute path" >&2; exit 2 ;; esac
if ! "$PYTHON" - "$D" "$SOCK" "$KILL_SWITCH_SMOKE" <<'PY'
import json, pathlib, sys
root, socket, stopped = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3] == "1"
quote = json.dumps
(root / "frankenterm.toml").write_text(
    '[[unix_domains]]\nname = "rc3-owned"\nsocket_path = ' + quote(socket) + '\nno_serve_automatically = true\n'
)
config = '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = ' + quote(socket) + '\n'
if stopped:
    config += ('[workflows]\nenabled = ["handle_compaction"]\nauto_run_allowlist = ["handle_compaction"]\n'
               'max_concurrent = 1\n[workflows.compaction_prompts.by_agent]\n'
               'claude_code = "RC3_FENCE_EFFECT\\n"\n')
(root / "ft.toml").write_text(config)
PY
then
  exit 2
fi
chmod 600 "$D/ft.toml"
HERMETIC_ENV=(
  "PATH=$BIN_DIR:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home"
  "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
  "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
  "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
  "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false"
)
file_sha() { "$PYTHON" - "$1" <<'PY'
import hashlib, pathlib, sys
digest = hashlib.sha256()
with pathlib.Path(sys.argv[1]).open('rb') as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b''):
        digest.update(chunk)
print(digest.hexdigest())
PY
}
CLI_SHA=$(file_sha "$FT") || exit 2
MUX_SHA=$(file_sha "$MUX") || exit 2
run_bounded() {
  # Drain both streams concurrently with finite byte/time budgets, preserving
  # actual bytes in the caller's retained files. Failure never retries an
  # ambiguous mutation; only this wrapper's own child is killed and reaped.
  env -i "${HERMETIC_ENV[@]}" "$PYTHON" -c '
import os, selectors, subprocess, sys, time
child = subprocess.Popen(sys.argv[1:], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
selector = selectors.DefaultSelector()
selector.register(child.stdout, selectors.EVENT_READ, 1)
selector.register(child.stderr, selectors.EVENT_READ, 2)
counts, deadline = {1: 0, 2: 0}, time.monotonic() + 25
try:
    while selector.get_map():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("25-second command deadline exceeded")
        for key, _ in selector.select(min(remaining, 0.1)):
            chunk = os.read(key.fd, 65536)
            if not chunk:
                selector.unregister(key.fileobj)
                key.fileobj.close()
                continue
            counts[key.data] += len(chunk)
            if counts[key.data] > 1048576:
                raise RuntimeError("command output exceeded one MiB per stream")
            target = sys.stdout.buffer if key.data == 1 else sys.stderr.buffer
            target.write(chunk)
            target.flush()
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("25-second command deadline exceeded")
    result = child.wait(timeout=remaining)
except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired) as error:
    print(f"owned candidate command failed: {error}; effect not confirmed", file=sys.stderr)
    sys.exit(124)
finally:
    if child.poll() is None:
        child.kill()
    child.wait()
    selector.close()
sys.exit(result if result >= 0 else 128 - result)
' "$@"
}
LOG="$D/smoke.log"
RECEIPT="$D/receipt.json"
STEPS="$D/steps.jsonl"
: > "$STEPS"
echo "smoke dir: $D"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$LOG"; }
plain() { printf '%s' "$1" | sed -E $'s/\x1b\\[[0-9;]*m//g' | tr -s '\n' ' '; }
step() { # name status detail
  local detail
  detail=$(plain "$3")
  jq -cn --arg n "$1" --arg s "$2" --arg d "$detail" '{name:$n,status:$s,detail:$d}' >> "$STEPS"
  log "step $1: $2 ($detail)"
}
owned_running() {
  local candidate="$1" running
  for running in $(jobs -pr); do [ "$running" = "$candidate" ] && return 0; done
  return 1
}
stop_children() {
  # Never `kill 0`: an unset pid would signal the whole process group (the
  # script included) before the receipt is written.
  if [ "$KILL_SWITCH_SMOKE" = 1 ] && [ -f "$D/phase" ]; then printf 'stop\n' > "$D/phase"; fi
  # A watcher whose mux is already gone does not honour SIGTERM (ft-yykm1)
  # and keeps polling, leaking a defunct child per poll; nine such orphans
  # once exhausted the maintainer Mac's process limit. Escalate to SIGKILL
  # after a short grace and report what was left behind.
  local pid deadline child_status status=0
  # Settle the watcher while its mux is still available for final capture.
  for pid in ${WATCH:-} ${MUX_PID:-}; do
    owned_running "$pid" && kill -TERM "$pid"
    deadline=$((SECONDS + 5))
    while owned_running "$pid" && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    if owned_running "$pid"; then
      log "child $pid ignored SIGTERM for 5s; sending SIGKILL"
      kill -9 "$pid"
      status=1
    fi
    if wait "$pid"; then child_status=0; else child_status=$?; fi
    log "owned child $pid reaped with status $child_status"
    if [ "$pid" = "${WATCH:-}" ] && [ "$child_status" -ne 0 ]; then
      status=1
    elif [ "$child_status" -ne 0 ] && [ "$child_status" -ne 143 ]; then
      status=1
    fi
  done
  WATCH=""
  MUX_PID=""
  return "$status"
}
finish() { # status
  local status="$1" final_cli_sha final_mux_sha
  run_bounded "$FT" --version > "$D/cli-version.txt" 2> "$D/cli-version.err" || status=fail
  run_bounded "$MUX" --version > "$D/mux-version.txt" 2> "$D/mux-version.err" || status=fail
  if ! final_cli_sha=$(file_sha "$FT") || ! final_mux_sha=$(file_sha "$MUX"); then
    step source_identity fail "candidate binary bytes could not be reread at closeout"
    status=fail
  elif [ "$final_cli_sha" != "$CLI_SHA" ] || [ "$final_mux_sha" != "$MUX_SHA" ]; then
    step source_identity fail "candidate binary bytes changed during the run"
    status=fail
  else
    step source_identity pass "candidate binary hashes unchanged through closeout"
  fi
  jq -n \
    --arg schema "ft.smoke.headless-mux-observe.v1" \
    --arg generated_at "$(date -u +%FT%TZ)" \
    --arg host "$(hostname)" \
    --arg commit "$RELEASE_COMMIT" \
    --arg source_authority "$SOURCE_AUTHORITY" \
    --arg cli_sha256 "$CLI_SHA" --arg mux_sha256 "$MUX_SHA" \
    --arg private_socket "$SOCK" \
    --argjson codec_version "$CODEC_VERSION" \
    --arg cli_version "$(head -1 "$D/cli-version.txt")" \
    --arg mux_version "$(head -1 "$D/mux-version.txt")" \
    --arg bin_dir "$BIN_DIR" \
    --arg status "$status" \
    --slurpfile steps "$STEPS" \
    '{schema:$schema,generated_at:$generated_at,host:$host,commit:$commit,source_authority:$source_authority,cli_sha256:$cli_sha256,mux_sha256:$mux_sha256,private_socket:$private_socket,codec_version:$codec_version,cli_version:$cli_version,mux_version:$mux_version,bin_dir:$bin_dir,status:$status,steps:$steps}' \
    > "$RECEIPT" || return 1
  log "receipt: $RECEIPT (status=$status)"
  [ "$status" = pass ]
}
fail() { # step-name detail
  step "$1" fail "$2"
  stop_children
  finish fail
  exit 1
}
trap stop_children EXIT
trap 'exit 1' HUP INT TERM

# This fixture records actual PTY input; it never fabricates an injector,
# policy decision, CLI result, or workflow event. Only captured terminal output
# can trigger the production watcher workflow.
if [ "$KILL_SWITCH_SMOKE" = 1 ]; then
  printf '0\n' > "$D/phase"
  cat > "$D/owned-pane.py" <<'PY'
import json, os, pathlib, select, sys, time, tty
root = pathlib.Path(sys.argv[1])
tty.setraw(sys.stdin.fileno())
(root / 'pane-owner.json').write_text(json.dumps({'pid': os.getpid(), 'parent_pid': os.getppid()}))
received = (root / 'pane-input.bin').open('ab', buffering=0)
deadline, previous, total = time.monotonic() + 180, '0', 0
os.write(1, b'\x1b]2;claude-code-owned-fixture\x07\x1b]133;A\x07fixture ready\r\n')
while time.monotonic() < deadline:
    phase = (root / 'phase').read_text().strip()
    if phase == 'stop':
        break
    if phase in {'1', '2', '3'} and phase != previous:
        previous = phase
        number = int(phase)
        os.write(1, f'Conversation compacted: {9000 + number} tokens to {4500 + number}\r\n'.encode())
        os.write(1, b'\x1b]133;A\x07fixture ready\r\n')
        (root / 'pane-phase.json').write_text(json.dumps({'phase': number, 'pid': os.getpid()}))
    ready, _, _ = select.select([0], [], [], 0.1)
    if ready:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        total += len(chunk)
        if total > 65536:
            raise RuntimeError('owned PTY input exceeded fixture cap')
        received.write(chunk)
        os.fsync(received.fileno())
        os.write(1, b'RC3_FIXTURE_INPUT_RECEIVED\r\n\x1b]133;A\x07')
else:
    raise RuntimeError('owned pane fixture deadline exceeded')
PY
  env -i "${HERMETIC_ENV[@]}" "$MUX" --config-file "$D/frankenterm.toml" \
    --daemonize=false --cwd "$D" -- "$PYTHON" "$D/owned-pane.py" "$D" > "$D/mux.log" 2>&1 &
else
  # Bare zsh: nothing rewrites the pane title after we set it.
  env -i "${HERMETIC_ENV[@]}" "$MUX" --config-file "$D/frankenterm.toml" \
    --daemonize=false --cwd "$D" -- /bin/zsh -f > "$D/mux.log" 2>&1 &
fi
MUX_PID=$!
# A stale socket file from an earlier server satisfies `-S`; wait for the lease
# file to name THIS server's pid so a client never dials a dead socket.
for _ in $(seq 1 150); do grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null && [ -S "$SOCK" ] && break; sleep 0.2; done
grep -q "pid=$MUX_PID" "$SOCK.lock" 2>/dev/null || fail mux_start "server pid $MUX_PID never took the socket lease: $(tail -3 "$D/mux.log" | tr '\n' ' ')"
step mux_start pass "pid $MUX_PID on $SOCK"

ft() { run_bounded "$FT" -c "$D/ft.toml" "$@"; }

ft doctor --json > "$D/doctor.json" 2> "$D/doctor.err" \
  || fail doctor "candidate doctor command failed; see doctor.err"
SOCK_ROW=$(jq -c '.checks[] | select(.name=="mux socket")' "$D/doctor.json" 2>/dev/null)
CONN_ROW=$(jq -c '.checks[] | select(.name=="WezTerm connection")' "$D/doctor.json" 2>/dev/null)
log "$SOCK_ROW"; log "$CONN_ROW"
jq -e '.checks[] | select(.name=="WezTerm connection") | .status == "ok"' "$D/doctor.json" > /dev/null \
  || fail doctor "did not reach the mux: $(tail -3 "$D/doctor.err" | tr '\n' ' ')"
step doctor pass "$(jq -r '.checks[] | select(.name=="WezTerm connection") | .detail' "$D/doctor.json")"

ft list --json > "$D/list.json" 2> "$D/list.err" \
  || fail list "candidate list command failed; see list.err"
PANE=$(jq -r '.[0].pane_id' "$D/list.json" 2>/dev/null)
[ -n "$PANE" ] && [ "$PANE" != "null" ] || fail list "no pane listed: $(tail -3 "$D/list.err" | tr '\n' ' ')"
jq -e 'length == 1' "$D/list.json" > /dev/null || fail list "private fixture must own exactly one pane"
step list pass "pane $PANE"

WATCH_ARGS=(watch --foreground --poll-interval 1000)
[ "$KILL_SWITCH_SMOKE" = 1 ] && WATCH_ARGS+=(--auto-handle)
env -i "${HERMETIC_ENV[@]}" "$FT" -c "$D/ft.toml" "${WATCH_ARGS[@]}" > "$D/watch.log" 2>&1 &
WATCH=$!
sleep 5
grep -a -q 'Started vendored pane streaming subscription' "$D/watch.log" \
  || fail watch "no streaming subscription within 5 s: $(tail -3 "$D/watch.log" | tr '\n' ' ')"
step watch pass "streaming subscription for pane $PANE"

if [ "$KILL_SWITCH_SMOKE" = 1 ]; then
  # Observe durable decisions independently of CLI success and of the pane
  # recorder. A timeout is failure, not a skipped/fallback mode.
  await_audit() {
    "$PYTHON" - "$D" "$PANE" "$1" "$2" <<'PY'
import json, pathlib, sqlite3, sys, time
root, pane, decision, phase = pathlib.Path(sys.argv[1]), int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
deadline = time.monotonic() + 35
while time.monotonic() < deadline:
    with sqlite3.connect((root / ".ft" / "ft.db").as_uri() + '?mode=ro', uri=True, timeout=1) as db:
        candidates = db.execute('SELECT a.id, a.policy_decision, a.rule_id, a.result, a.actor_id, e.extracted FROM audit_actions a JOIN workflow_executions w ON w.id=a.actor_id JOIN events e ON e.id=w.trigger_event_id WHERE a.pane_id=? AND a.actor_kind=? AND a.action_kind=? AND a.policy_decision=? AND w.workflow_name=? ORDER BY a.id', (pane, 'workflow', 'send_text', decision, 'handle_compaction')).fetchall()
    rows = [row for row in candidates if json.loads(row[5] or '{}').get('tokens_before') == str(9000 + phase) and json.loads(row[5] or '{}').get('tokens_after') == str(4500 + phase)]
    if rows:
        if decision == 'deny' and rows[-1][2] != 'policy.kill_switch':
            raise RuntimeError(f'wrong denial source: {rows[-1]!r}')
        print(json.dumps({'decision': decision, 'trigger_phase': phase, 'rows': rows}))
        break
    time.sleep(0.2)
else:
    raise RuntimeError(f'no {decision} workflow audit tied to real trigger phase {phase} within deadline')
PY
  }
  await_input() {
    "$PYTHON" - "$D/pane-input.bin" "$1" <<'PY'
import hashlib, pathlib, sys, time
path, required = pathlib.Path(sys.argv[1]), int(sys.argv[2])
deadline, previous, stable_since = time.monotonic() + 10, None, time.monotonic()
while time.monotonic() < deadline:
    data = path.read_bytes()
    count = data.count(b'RC3_FENCE_EFFECT')
    if count > required:
        raise RuntimeError(f'duplicate/replayed effect: {count} markers, expected {required}')
    if data != previous:
        previous, stable_since = data, time.monotonic()
    if count == required and time.monotonic() - stable_since >= 0.6:
        print(hashlib.sha256(data).hexdigest())
        break
    time.sleep(0.1)
else:
    raise RuntimeError('allowed effect did not reach stable owned PTY input')
PY
  }
  printf '1\n' > "$D/phase"
  await_audit allow 1 > "$D/baseline-audit.json" 2> "$D/baseline-audit.err" \
    || fail kill_switch_baseline "no real workflow allow; see baseline-audit.err and watch.log"
  BASE_INPUT_SHA=$(await_input 1) \
    || fail kill_switch_baseline "allowed workflow did not reach stable owned PTY input"
  step kill_switch_baseline pass "watcher $WATCH delivered a compaction workflow to owned pane $PANE"

  ft robot --format json kill-switch trip --level hard-stop --reason rc3-owned-trip \
    > "$D/trip.json" 2> "$D/trip.err" || fail kill_switch_trip "operator CLI exited unsuccessfully; see trip.err"
  jq -e '.ok == true and .data.persisted == true and .data.level == "hard_stop" and .data.revision > 0 and .data.fenced_owner == "policy_gated_injector" and .data.pre_admitted_remote_effects == "not_proven_settled"' "$D/trip.json" > /dev/null \
    || fail kill_switch_trip "trip not durably acknowledged with the required scope"
  printf '2\n' > "$D/phase"
  await_audit deny 2 > "$D/denial-audit.json" 2> "$D/denial-audit.err" \
    || fail kill_switch_denial "same watcher did not persist kill-switch denial; see denial-audit.err and watch.log"
  [ "$(file_sha "$D/pane-input.bin")" = "$BASE_INPUT_SHA" ] \
    || fail kill_switch_denial "stopped workflow changed actual PTY input bytes"
  owned_running "$WATCH" || fail kill_switch_denial "original watcher exited"
  step kill_switch_denial pass "same watcher $WATCH persisted policy.kill_switch denial; PTY input unchanged"

  ft robot --format json kill-switch reset --by rc3-owned-operator \
    > "$D/reset.json" 2> "$D/reset.err" || fail kill_switch_reset "operator reset exited unsuccessfully; see reset.err"
  jq -e --slurpfile trip "$D/trip.json" '.ok == true and .data.persisted == true and .data.level == "disarmed" and .data.revision > $trip[0].data.revision' "$D/reset.json" > /dev/null \
    || fail kill_switch_reset "reset did not advance durable revision"
  sleep 3
  [ "$(file_sha "$D/pane-input.bin")" = "$BASE_INPUT_SHA" ] \
    || fail kill_switch_reset "reset replayed denied input without a fresh trigger"
  printf '3\n' > "$D/phase"
  await_audit allow 3 > "$D/recovery-audit.json" 2> "$D/recovery-audit.err" \
    || fail kill_switch_recovery "fresh workflow did not succeed after reset"
  await_input 2 > "$D/recovery-input.sha256" \
    || fail kill_switch_recovery "fresh allowed workflow did not settle in the owned PTY recorder"
  [ "$(file_sha "$D/pane-input.bin")" != "$BASE_INPUT_SHA" ] \
    || fail kill_switch_recovery "fresh allowed workflow did not reach PTY input"
  owned_running "$WATCH" || fail kill_switch_recovery "watcher was replaced or exited"
  step kill_switch_recovery pass "fresh compaction trigger delivered after reset; no rejected send replay"
  stop_children || fail shutdown "owned watcher did not settle cleanly or a child required SIGKILL"
  finish pass || exit 1
  echo "PASS: owned watcher kill-switch admission (remote settlement remains unproven; evidence in $D)"
  exit 0
fi

ft send --no-paste "$PANE" 'printf "\033]2;codex\007"' > "$D/send1.log" 2>&1 || fail send_title "$(tail -2 "$D/send1.log" | tr '\n' ' ')"
sleep 3
ft send --no-paste "$PANE" "echo \"You've reached your usage limit. try again at 3:00 PM.\"" > "$D/send2.log" 2>&1 || fail send_limit "$(tail -2 "$D/send2.log" | tr '\n' ' ')"
step send pass "title set to codex; usage-limit line sent"
sleep 10

ft events -f json -l 5 > "$D/events.json" 2> "$D/events.err" \
  || fail detect "candidate events command failed; see events.err"
jq -c '.[] | {id, rule_id, agent_type, severity, extracted, matched_text}' "$D/events.json" 2>/dev/null | tee -a "$LOG"
jq -e 'any(.[]; .rule_id == "codex.usage.reached")' "$D/events.json" > /dev/null \
  || fail detect "no codex.usage.reached event ($(grep -c . "$D/watch.log") watch log lines in $D)"
step detect pass "$(jq -c '[.[] | select(.rule_id=="codex.usage.reached")][0] | {id, extracted}' "$D/events.json")"

# Drain the watcher before assessing durability. Captures still queued when
# detection succeeds can fail during shutdown and must not escape this gate.
stop_children || fail shutdown "owned watcher did not settle cleanly or a child required SIGKILL"

DROPPED=$(grep -a -c 'Failed to persist segment' "$D/watch.log")
RESYNCS=$(grep -a -c 'Sequence discontinuity' "$D/watch.log")
if [ "$DROPPED" != "0" ] || [ "$RESYNCS" != "0" ]; then
  fail durability "dropped segments: $DROPPED, sequence resyncs: $RESYNCS (ft-xxfwy.32)"
fi
step durability pass "dropped segments 0, sequence resyncs 0"

finish pass || exit 1
echo "PASS: observe->detect on a real headless mux (evidence in $D)"
