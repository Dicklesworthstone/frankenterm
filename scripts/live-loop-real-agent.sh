#!/usr/bin/env bash
# Live-loop proof with a REAL Claude Code pane (ft-xxfwy.9 seed).
#
# A private mux runs one Claude Code pane (the operator's real install and
# account, cwd = this repo). The script submits a warm-up prompt, then the
# agent's own /compact. `ft watch --auto-handle` must detect the real
# compaction, handle_compaction must submit its refresh prompt, and the agent
# must answer it. Asserted, each non-zero on failure:
#   1. claude_code.compaction detected from the agent's own output
#   2. the workflow's send_text audit row records verified state `submitted`
#   3. the agent took a turn on the workflow prompt: the prompt is in the
#      transcript, a completed-turn marker follows it, the composer is empty
#   4. the operator's ~/.local/bin/claude link never points into a temp dir
#   5. a real Codex pane, spawned by `ft robot profile apply`, answers a
#      `ft robot send --verify-submit` whose receipt says `submitted`
#      (reported as SKIP, never as a pass, when no codex binary is installed)
#   6. a zsh pane with OSC 133 prompt integration accepts a plain
#      `ft robot send` on live prompt evidence and runs the command
#
# Spends a few model turns on the operator's account, so it only runs with
# LIVE_LOOP_REAL_AGENTS=1. The mux, socket and ft state are private; only the
# agent sees the real HOME (its auto-updater is disabled).
#
# usage: LIVE_LOOP_REAL_AGENTS=1 [LIVE_LOOP_RETAIN=1] scripts/live-loop-real-agent.sh [BIN_DIR]
# LIVE_LOOP_RETAIN=1 keeps a passing receipt as docs/attestations/proofs/live-loop-tier1-seed.json.
set -uo pipefail
umask 077

[[ "${LIVE_LOOP_REAL_AGENTS:-0}" == 1 ]] || { echo "set LIVE_LOOP_REAL_AGENTS=1 (spends real model turns)" >&2; exit 2; }
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$(cd "${1:-$REPO_ROOT/target/debug}" && pwd)"
FT_BIN="$BIN/ft"; MUX_BIN="$BIN/frankenterm-mux-server"
CLAUDE_BIN="${CLAUDE_BIN:-$HOME/.local/bin/claude}"
CODEX_BIN="${CODEX_BIN:-$(command -v codex || true)}"
for bin in "$FT_BIN" "$MUX_BIN" "$CLAUDE_BIN"; do [[ -x "$bin" ]] || { echo "missing $bin" >&2; exit 2; }; done
OUT="$REPO_ROOT/tests/e2e/artifacts/live-loop-real/$(date +%Y%m%dT%H%M%S)"
mkdir -p "$OUT"
CLAUDE_LINK_BEFORE=$(readlink "$CLAUDE_BIN" 2> /dev/null || echo "$CLAUDE_BIN")

D=$(mktemp -d /tmp/ftlr-XXXXXX)
mkdir -p "$D/.ft" "$D/home" "$D/tmp" "$D/runtime"
chmod 700 "$D" "$D/.ft" "$D/runtime"
SOCK="$D/mux.sock"
printf '[[unix_domains]]\nname = "lr"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
cat > "$D/ft.toml" << EOF
[storage]
db_path = "ft.db"
[vendored]
mux_socket_path = "$SOCK"
[workflows]
enabled = ["handle_compaction"]
auto_run_allowlist = ["handle_compaction"]
max_concurrent = 1
[workflows.compaction_prompts.by_agent]
claude_code = "Reply with the word READY-TIER1 and nothing else.\n"
EOF
# The agent gets the operator's real, consistent HOME/XDG: a mixed env lets the
# Claude Code updater relink ~/.local/bin/claude into a temp dir.
cat > "$D/agent.sh" << EOF
#!/bin/bash
cd "$REPO_ROOT"
exec env -i HOME="$HOME" PATH="$(dirname "$CLAUDE_BIN"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin" \\
  TERM=xterm-256color LANG=en_US.UTF-8 USER="$USER" DISABLE_AUTOUPDATER=1 "$CLAUDE_BIN" --model haiku
EOF
chmod 700 "$D/agent.sh"
# Shell integration the way `ft setup shell` wires it: OSC 133 prompt, command
# and exit-status marks from zsh hooks.
mkdir -p "$D/zdot"
cat > "$D/zdot/.zshrc" << 'EOF'
precmd() { print -n "\e]133;D;$?\a" }
preexec() { print -n "\e]133;C\a" }
PS1=$'%{\e]133;A\a%}tier1-shell> %{\e]133;B\a%}'
EOF
cat > "$D/shell.sh" << EOF
#!/bin/bash
cd "$REPO_ROOT"
exec env -i HOME="$D/home" ZDOTDIR="$D/zdot" PATH=/usr/bin:/bin TERM=xterm-256color LANG=en_US.UTF-8 /bin/zsh -i
EOF
chmod 700 "$D/shell.sh"
if [[ -x "$CODEX_BIN" ]]; then
  cat > "$D/codex.sh" << EOF
#!/bin/bash
cd "$REPO_ROOT"
exec env -i HOME="$HOME" PATH="$(dirname "$CODEX_BIN"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin" \\
  TERM=xterm-256color LANG=en_US.UTF-8 USER="$USER" "$CODEX_BIN"
EOF
  chmod 700 "$D/codex.sh"
fi
ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home" "TMPDIR=$D/tmp"
  "XDG_RUNTIME_DIR=$D/runtime" "WEZTERM_UNIX_SOCKET=$SOCK" "FRANKENTERM_UNIX_SOCKET=$SOCK"
  "FRANKENTERM_CONFIG_FILE=$D/frankenterm.toml" "FT_WORKSPACE=$D"
  "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
ft() { env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" "$@"; }
PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2> /dev/null; done
  for pid in "${PIDS[@]}"; do wait "$pid" 2> /dev/null; done
}
trap cleanup EXIT

CHECKS=()
check() { # name exit-status detail
  local ok=false; [[ "$2" == 0 ]] && ok=true
  echo "$([[ $ok == true ]] && echo PASS || echo FAIL) $1 ${3:+- $3}"
  CHECKS+=("$(python3 -c 'import json,sys;print(json.dumps({"name":sys.argv[1],"ok":sys.argv[2]=="true","detail":sys.argv[3]}))' "$1" "$ok" "${3:-}")")
}

env -i "${ENVV[@]}" "$MUX_BIN" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- "$D/agent.sh" \
  > "$OUT/mux.log" 2>&1 &
PIDS+=("$!")
MUXPID=$!
for _ in $(seq 1 150); do grep -q "pid=$MUXPID" "$SOCK.lock" 2> /dev/null && [[ -S "$SOCK" ]] && break; sleep 0.2; done
env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" watch --foreground --auto-handle --poll-interval 500 \
  > "$OUT/watch.log" 2>&1 &
PIDS+=("$!")
P=$(ft list --json 2> /dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pane_id"])')

idle() { # screen shows a reply and no running turn
  for _ in $(seq 1 60); do
    sleep 5
    ft get-text "$P" --tail 60 > "$OUT/$1.txt" 2> /dev/null
    grep -q "⏺" "$OUT/$1.txt" && ! grep -q "esc to interrupt" "$OUT/$1.txt" && return 0
  done
  return 1
}
sleep 15
ft send "$P" "Say hi in two words." > /dev/null 2>&1
idle warmup || echo "warm-up reply not seen"
ft send "$P" "/compact" > /dev/null 2>&1
# Turn taken on the workflow prompt: its transcript line, then a finished-turn
# marker ("Worked for 12s · done"), then an empty composer. The reply wording
# is the model's; only the turn is asserted.
answered() {
  python3 - "$OUT/screen.txt" << 'PY'
import re, sys
lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
prompt = [i for i, line in enumerate(lines) if "Reply with the word READY-TIER1" in line and "❯" in line]
if not prompt:
    sys.exit(1)
after = lines[prompt[-1] + 1:]
done = any(re.search(r"for \d+(m \d+)?s · done", line) for line in after)
composer_empty = any(line.strip() == "❯" for line in after)
sys.exit(0 if done and composer_empty else 1)
PY
}
for _ in $(seq 1 48); do
  sleep 5
  ft get-text "$P" --tail 200 > "$OUT/screen.txt" 2> /dev/null
  answered && break
done

ft robot --format json events --limit 100 > "$OUT/events.json" 2> /dev/null
python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));sys.exit(0 if any(e["rule_id"]=="claude_code.compaction" for e in d["data"]["events"]) else 1)' \
  "$OUT/events.json"
check "real_compaction_detected" $? "claude_code.compaction"

ft audit --format json --actor workflow --limit 50 > "$OUT/audit-workflow.json" 2> /dev/null
python3 -c '
import json, sys
rows = [r for r in json.load(open(sys.argv[1])) if r.get("action_kind") == "send_text"]
states = [json.loads(r.get("verification_summary") or "{}").get("state") for r in rows]
sys.exit(0 if "submitted" in states else 1)' "$OUT/audit-workflow.json"
check "workflow_send_verified_submitted" $? "audit send_text verification state"

answered
check "agent_took_a_turn_on_workflow_prompt" $? "prompt, finished turn, empty composer"

# The operator's own sessions may update Claude Code meanwhile; what must never
# happen is the link moving into a temp dir.
if [[ -x "$CODEX_BIN" ]]; then
  ft robot profile create codex_ws --command "$D/codex.sh" > "$OUT/profile-create-codex.json" 2>&1
  ft robot profile apply codex_ws --count 1 > "$OUT/profile-apply-codex.json" 2>&1
  CP=$(python3 -c 'import json,sys;t=open(sys.argv[1]).read();print(json.loads(t[t.index("{"):])["data"]["panes_spawned"][0])' \
    "$OUT/profile-apply-codex.json" 2> /dev/null)
  sleep 20
  ft robot send --verify-submit "$CP" "Reply with the word PONG spelled backwards and nothing else" \
    > "$OUT/codex-send.json" 2> "$OUT/codex-send.err"
  sleep 40
  ft get-text "$CP" --tail 60 > "$OUT/codex-screen.txt" 2> /dev/null
  python3 - "$OUT/codex-send.json" "$OUT/codex-screen.txt" << 'PY'
import json, sys
text = open(sys.argv[1]).read()
submit = (json.loads(text[text.index("{"):]).get("data") or {}).get("submit") or {}
screen = open(sys.argv[2], encoding="utf-8", errors="replace").read().splitlines()
answered = any(line.strip() in ("• GNOP", "GNOP") for line in screen)
sys.exit(0 if submit.get("state") == "submitted" and answered else 1)
PY
  check "codex_verified_submit_answered" $? "pane ${CP:-?}"
else
  echo "SKIP codex_verified_submit_answered - no codex binary"
fi

ft robot profile create shell_ws --command "$D/shell.sh" > "$OUT/profile-create-shell.json" 2>&1
ft robot profile apply shell_ws --count 1 > "$OUT/profile-apply-shell.json" 2>&1
SP=$(python3 -c 'import json,sys;t=open(sys.argv[1]).read();print(json.loads(t[t.index("{"):])["data"]["panes_spawned"][0])' \
  "$OUT/profile-apply-shell.json" 2> /dev/null)
sleep 8
MARK="TIER1_SHELL_$$"
ft robot send "$SP" "echo $MARK-ran" > "$OUT/shell-send.json" 2> "$OUT/shell-send.err"
sleep 3
ft get-text "$SP" --tail 60 > "$OUT/shell-screen.txt" 2> /dev/null
python3 - "$OUT/shell-send.json" "$OUT/shell-screen.txt" "$MARK-ran" << 'PY'
import json, sys
text = open(sys.argv[1]).read()
injection = (json.loads(text[text.index("{"):]).get("data") or {}).get("injection") or {}
ran = any(line.strip() == sys.argv[3] for line in open(sys.argv[2], encoding="utf-8", errors="replace"))
sys.exit(0 if injection.get("status") == "allowed" and ran else 1)
PY
check "shell_send_allowed_on_live_prompt_and_ran" $? "pane ${SP:-?}"

CLAUDE_LINK_AFTER=$(readlink "$CLAUDE_BIN" 2> /dev/null || echo "$CLAUDE_BIN")
case "$CLAUDE_LINK_AFTER" in /tmp/* | /private/tmp/* | /var/folders/*) false ;; *) true ;; esac
check "operator_claude_install_not_in_temp" $? "$CLAUDE_LINK_BEFORE -> $CLAUDE_LINK_AFTER"

python3 - "$OUT/receipt.json" "$("$FT_BIN" --version 2> /dev/null)" "$("$CLAUDE_BIN" --version 2> /dev/null)" "${CHECKS[@]}" << 'PY'
import json, platform, subprocess, sys, time
checks = [json.loads(c) for c in sys.argv[4:]]
receipt = {
    "schema": "ft.live-loop-proof.v1", "tier": "1-seed", "adapter": "live-mux",
    "panes": "real Claude Code (haiku, real /compact) + real Codex + zsh with OSC 133",
    "ft_version": sys.argv[2].strip(), "agent_version": sys.argv[3].strip(),
    "host": platform.node(), "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "commit": subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
    "crates_dirty": bool(subprocess.run(["git", "status", "--short", "--", "crates"], capture_output=True, text=True).stdout.strip()),
    "status": "pass" if checks and all(c["ok"] for c in checks) else "fail",
    "checks": checks,
}
json.dump(receipt, open(sys.argv[1], "w"), indent=2)
print(f"live-loop real agent: {receipt['status']} ({sum(c['ok'] for c in checks)}/{len(checks)})")
PY
python3 -c 'import json,sys;sys.exit(0 if json.load(open(sys.argv[1]))["status"]=="pass" else 1)' "$OUT/receipt.json" || exit 1
if [[ "${LIVE_LOOP_RETAIN:-0}" == 1 ]]; then
  cp "$OUT/receipt.json" "$REPO_ROOT/docs/attestations/proofs/live-loop-tier1-seed.json"
fi
