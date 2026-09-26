#!/usr/bin/env bash
# test_ft_attaches_to_running_gui.sh — native macOS e2e: ft talks to a running
# FrankenTerm GUI of the same build (ft-xxfwy.8).
#
# Launches frankenterm-gui under its own window class (--class ...frankenterm.rc,
# --skip-config), so the operator's daily FrankenTerm.app, its published socket
# and its panes are never touched. ft runs with an isolated HOME/XDG/workspace and
# dials the dev GUI's published socket explicitly (discovery would rank the daily
# app's default-class symlink first). Steps, each asserted:
#   1. versions: ft and frankenterm-gui report the same commit
#   2. ft doctor: mux socket ok and mux generation "paired"
#   3. ft robot profile apply opens a pane in the GUI printing a random marker
#   4. ft robot state lists that pane
#   5. ft robot get-text returns the marker
#   6. after one ft watch pass, ft robot search finds the marker
# Artifacts: tests/e2e/artifacts/gui-attach/<ts>/ (versions, doctor, state,
# get-text, search, watch log, steps.jsonl, receipt.json).
#
# usage: FT_BIN_DIR=<dir with ft + frankenterm-gui> tests/e2e/test_ft_attaches_to_running_gui.sh
set -uo pipefail
umask 077

[[ "$(uname -s)" == Darwin ]] || { echo "SKIP: native macOS only" >&2; exit 2; }
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$(cd "${FT_BIN_DIR:-$REPO_ROOT/target/debug}" && pwd)"
FT_BIN="$BIN/ft"; GUI_BIN="$BIN/frankenterm-gui"
for bin in "$FT_BIN" "$GUI_BIN"; do [[ -x "$bin" ]] || { echo "SKIP: $bin not built" >&2; exit 2; }; done
OUT="$REPO_ROOT/tests/e2e/artifacts/gui-attach/$(date +%Y%m%dT%H%M%S)"
mkdir -p "$OUT"
CLASS=com.dicklesworthstone.frankenterm.rc

D=$(mktemp -d /tmp/ftga-XXXXXX)
mkdir -p "$D/.ft" "$D/home" "$D/config" "$D/cache" "$D/data" "$D/state" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/.ft" "$D/runtime"
PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2> /dev/null; done
  for pid in "${PIDS[@]}"; do wait "$pid" 2> /dev/null; done
}
trap cleanup EXIT

: > "$OUT/steps.jsonl"
PASS=0; FAIL=0
step() { # name exit-status started-at detail
  local ok=false elapsed
  [[ "$2" == 0 ]] && ok=true && PASS=$((PASS + 1)) || FAIL=$((FAIL + 1))
  elapsed=$(python3 -c 'import sys,time;print(int((time.time()-float(sys.argv[1]))*1000))' "$3")
  echo "$([[ $ok == true ]] && echo PASS || echo FAIL) $1 ${4:+- $4}"
  python3 -c 'import json,sys;print(json.dumps({"step":sys.argv[1],"ok":sys.argv[2]=="true","elapsed_ms":int(sys.argv[3]),"detail":sys.argv[4]}))' \
    "$1" "$ok" "$elapsed" "${4:-}" >> "$OUT/steps.jsonl"
}
now() { python3 -c 'import time;print(time.time())'; }

# The dev GUI: separate class, no user config, own process.
"$GUI_BIN" --skip-config start --class "$CLASS" --always-new-process > "$OUT/gui.log" 2>&1 &
GUI_PID=$!; PIDS+=("$GUI_PID")
SOCK=""
for _ in $(seq 1 150); do
  for candidate in "$HOME/.local/share/frankenterm/frankenterm-gui-sock-$GUI_PID" \
    "$HOME/Library/Application Support/frankenterm/frankenterm-gui-sock-$GUI_PID"; do
    [[ -S "$candidate" ]] && SOCK="$candidate" && break 2
  done
  sleep 0.2
done
[[ -n "$SOCK" ]] || { echo "FAIL: dev GUI (pid $GUI_PID) published no socket" >&2; exit 1; }
echo "dev GUI pid $GUI_PID, socket $SOCK"

printf '[storage]\ndb_path = "ft.db"\n[vendored]\nmux_socket_path = "%s"\n' "$SOCK" > "$D/ft.toml"
ENVV=("PATH=$BIN:/usr/bin:/bin:/usr/sbin:/sbin" "LANG=C" "HOME=$D/home" "SHELL=/bin/zsh"
  "XDG_CONFIG_HOME=$D/config" "XDG_CACHE_HOME=$D/cache" "XDG_DATA_HOME=$D/data"
  "XDG_STATE_HOME=$D/state" "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "FT_WORKSPACE=$D" "FT_WEZTERM_CLI=$D/external-cli-disabled" "FT_METRICS_ENABLED=false")
ft() { env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" "$@"; }

# 1. Same generation.
t=$(now)
FT_VERSION=$("$FT_BIN" --version 2> /dev/null)
GUI_VERSION=$("$GUI_BIN" --version 2> /dev/null)
python3 -c 'import json,sys;json.dump({"ft":sys.argv[1],"gui":sys.argv[2],"socket":sys.argv[3]},open(sys.argv[4],"w"))' \
  "$FT_VERSION" "$GUI_VERSION" "$SOCK" "$OUT/versions.json"
FT_COMMIT=$(grep -oE '[0-9a-f]{40}' <<< "$FT_VERSION" | head -1)
[[ -n "$FT_COMMIT" ]] && grep -q "${FT_COMMIT:0:9}" <<< "$GUI_VERSION"
step "same_generation" $? "$t" "ft ${FT_COMMIT:0:9}; gui: $GUI_VERSION"

# 2. Doctor sees the GUI's mux and pairs with it.
t=$(now)
ft doctor --json > "$OUT/doctor.json" 2> "$OUT/doctor.err"
python3 - "$OUT/doctor.json" << 'PY'
import json, sys
t = open(sys.argv[1]).read()
checks = {c.get("name"): c for c in json.loads(t[t.index("{"):]).get("checks", [])}
sock, gen = checks.get("mux socket", {}), checks.get("mux generation", {})
ok = sock.get("status") == "ok" and gen.get("status") == "ok" and "paired" in str(gen.get("detail"))
print(f"socket: {sock.get('detail')} | generation: {gen.get('detail')}")
sys.exit(0 if ok else 1)
PY
step "doctor_pairs_with_gui_mux" $? "$t" "$(python3 -c 'import json,sys;t=open(sys.argv[1]).read();c={x["name"]:x for x in json.loads(t[t.index("{"):])["checks"]};print(c.get("mux generation",{}).get("detail"))' "$OUT/doctor.json" 2> /dev/null)"

# 3. A pane opened through ft in the GUI.
t=$(now)
MARKER="RC_MARKER_$RANDOM$RANDOM"
ft robot profile create gui_attach --command "printf '%s\n' $MARKER; exec /bin/zsh -f" > "$OUT/profile-create.json" 2>&1
ft robot profile apply gui_attach --count 1 > "$OUT/profile-apply.json" 2>&1
PANE=$(python3 -c 'import json,sys;t=open(sys.argv[1]).read();print(json.loads(t[t.index("{"):])["data"]["panes_spawned"][0])' \
  "$OUT/profile-apply.json" 2> /dev/null)
[[ -n "$PANE" ]]
step "profile_apply_opens_a_gui_pane" $? "$t" "pane ${PANE:-none}"
sleep 3

# 4. robot state lists it.
t=$(now)
ft robot --format json state > "$OUT/state.json" 2> /dev/null
python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));p=d.get("data",d);p=p.get("panes",p) if isinstance(p,dict) else p;sys.exit(0 if any(str(x.get("pane_id"))==sys.argv[2] for x in p) else 1)' \
  "$OUT/state.json" "${PANE:--1}"
step "robot_state_lists_the_pane" $? "$t" "pane ${PANE:-none}"

# 5. get-text returns the marker.
t=$(now)
ft robot --format json get-text "${PANE:-0}" > "$OUT/get-text.json" 2> /dev/null
grep -q "$MARKER" "$OUT/get-text.json"
step "robot_get_text_returns_the_marker" $? "$t" "$MARKER"

# 6. One watcher pass, then search finds it.
t=$(now)
env -i "${ENVV[@]}" "$FT_BIN" -c "$D/ft.toml" watch --foreground --poll-interval 500 > "$OUT/watch.log" 2>&1 &
WATCH_PID=$!; PIDS+=("$WATCH_PID")
FOUND=1
for _ in $(seq 1 30); do
  sleep 1
  ft robot --format json search "$MARKER" --limit 10 > "$OUT/search.json" 2> /dev/null
  grep -q "$MARKER" "$OUT/search.json" && FOUND=0 && break
done
step "robot_search_finds_the_marker_after_a_watch_pass" $FOUND "$t" "$MARKER"
kill "$WATCH_PID" 2> /dev/null

python3 - "$OUT/receipt.json" "$OUT/steps.jsonl" "$FT_VERSION" "$GUI_VERSION" "$SOCK" "$(basename "$BIN")" << 'PY'
import json, platform, subprocess, sys, time
steps = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
receipt = {
    "schema": "ft.gui-attach.v1", "host": platform.node(),
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "commit": subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
    "ft_version": sys.argv[3].strip(), "gui_version": sys.argv[4].strip(),
    "socket": sys.argv[5], "socket_source": "explicit_config (dev GUI under its own window class)",
    "build_profile": sys.argv[6],
    "status": "pass" if steps and all(s["ok"] for s in steps) else "fail",
    "steps": steps,
}
json.dump(receipt, open(sys.argv[1], "w"), indent=2)
print(f"gui attach: {receipt['status']} ({sum(s['ok'] for s in steps)}/{len(steps)})")
PY
[[ "$FAIL" == 0 ]]
