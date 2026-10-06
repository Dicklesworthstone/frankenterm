#!/usr/bin/env bash
# Soak harness for an isolated dev FrankenTerm GUI on macOS (ft-yccm0.1.7,
# the M.6 footprint gate).
#
# Usage: scripts/mac-gui-soak.sh --gui-bin PATH [options]
#   --duration D         total run time, e.g. 90s, 30m, 24h (default 24h)
#   --sample-interval D  footprint bundle interval (default 5m)
#   --idle D             idle pause after each corpus pass (default 60s)
#   --resize-interval D  window resize interval (default 10m)
#   --corpus-dir DIR     extra corpora to cat in rotation (for example the
#                        M.1 ingest corpora written by the frankenterm-term
#                        ingest_throughput example with --gen-only)
#   --no-atlas-growth    leave out the generated atlas-growth corpus
#   --gpu-budget-mib N   GPU-owned budget for the verdict (default 1024)
#   --out DIR            run directory (default ./soak-runs/<UTC stamp>)
#   --self-test          2-minute run, a bundle every 15 s, resize every 30 s
#   --dry-run            print the plan and the isolated environment only
#
# Workload, inside the GUI's own pane: the generated corpora (operator-format
# 256-color emoji frames, plain seq lines, scroll-region churn and, unless
# disabled, an atlas-growth corpus of ~32k distinct CJK/Hangul glyphs in four
# styles plus double-size lines), any --corpus-dir files, and an idle pause
# after each pass. The window is resized between two sizes through System
# Events by process id (never keystrokes, never the frontmost window); if
# macOS denies Accessibility access the receipt records resize as unavailable.
# Viewport scrolling is not driven: synthetic scroll input needs keyboard or
# mouse injection into the focused window, which could reach the operator's
# terminals. The scroll-region corpus exercises in-pane scrolling instead.
#
# Verdict: scripts/mac-gui-footprint.sh growth over the bundles (slope test on
# the last half, GPU budget). Runs shorter than 1 h report it as advisory,
# because start-up warm-up dominates a short tail. receipt.json and run.log
# are written to the run directory. Exit 0 pass, 1 fail, 2 usage/setup error.
#
# Isolation (the live-mux hazard): the GUI starts under `env -i` with its own
# HOME, XDG dirs and TMPDIR, so no WEZTERM_*/FT_*/FRANKENTERM_* variable or
# socket of the operator's session is inherited, and with
# --always-new-process so it never hands off to a running GUI. Before launch a
# read-only `ft list` probe under the same environment must find no mux. The
# harness only ever signals the exact PID it started.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FOOTPRINT="$SCRIPT_DIR/mac-gui-footprint.sh"

GUI_BIN=""
DURATION="24h"
SAMPLE_INTERVAL="5m"
IDLE="60s"
RESIZE_INTERVAL="10m"
CORPUS_DIR=""
ATLAS_GROWTH=1
GPU_BUDGET_MIB=1024
OUT=""
SELF_TEST=0
DRY_RUN=0

die() {
  echo "mac-gui-soak: $*" >&2
  exit 2
}

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed -e '/^set -euo/d' -e 's/^# \{0,1\}//'
}

seconds() {
  local value="$1"
  case "$value" in
    *h) echo $(( ${value%h} * 3600 )) ;;
    *m) echo $(( ${value%m} * 60 )) ;;
    *s) echo "${value%s}" ;;
    *[!0-9]*|"") die "bad duration $value" ;;
    *) echo "$value" ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --gui-bin) GUI_BIN="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --sample-interval) SAMPLE_INTERVAL="$2"; shift 2 ;;
    --idle) IDLE="$2"; shift 2 ;;
    --resize-interval) RESIZE_INTERVAL="$2"; shift 2 ;;
    --corpus-dir) CORPUS_DIR="$2"; shift 2 ;;
    --no-atlas-growth) ATLAS_GROWTH=0; shift ;;
    --gpu-budget-mib) GPU_BUDGET_MIB="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument $1" ;;
  esac
done

if [[ "$SELF_TEST" == 1 ]]; then
  DURATION="2m"
  SAMPLE_INTERVAL="15s"
  IDLE="5s"
  RESIZE_INTERVAL="30s"
fi

[[ "$(uname -s)" == "Darwin" ]] || die "macOS only"
[[ -n "$GUI_BIN" ]] || die "--gui-bin is required"
[[ -x "$GUI_BIN" ]] || die "--gui-bin $GUI_BIN is not executable"
GUI_BIN="$(cd "$(dirname "$GUI_BIN")" && pwd)/$(basename "$GUI_BIN")"
[[ -x "$FOOTPRINT" ]] || die "missing $FOOTPRINT"

DURATION_S="$(seconds "$DURATION")"
SAMPLE_S="$(seconds "$SAMPLE_INTERVAL")"
IDLE_S="$(seconds "$IDLE")"
RESIZE_S="$(seconds "$RESIZE_INTERVAL")"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="${OUT:-$PWD/soak-runs/$STAMP}"
mkdir -p "$RUN"
RUN="$(cd "$RUN" && pwd)"
ISO_HOME="$RUN/home"
RUNTIME_DIR="$ISO_HOME/.local/share/frankenterm"
BUNDLES="$RUN/bundles"
CORPUS="$RUN/corpus"
STOP="$RUN/stop"
LOG="$RUN/run.log"
mkdir -p "$ISO_HOME/.config" "$ISO_HOME/.local/share" "$ISO_HOME/.cache" "$RUN/tmp" "$RUN/xdg-runtime" \
  "$BUNDLES" "$CORPUS"

log() {
  printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG" >&2
}

ISO_ENV=(
  env -i
  "HOME=$ISO_HOME"
  "USER=$(id -un)"
  "LOGNAME=$(id -un)"
  "PATH=/usr/bin:/bin:/usr/sbin:/sbin"
  "LANG=en_US.UTF-8"
  "TMPDIR=$RUN/tmp/"
  "XDG_CONFIG_HOME=$ISO_HOME/.config"
  "XDG_DATA_HOME=$ISO_HOME/.local/share"
  "XDG_CACHE_HOME=$ISO_HOME/.cache"
  "XDG_RUNTIME_DIR=$RUN/xdg-runtime"
)

generate_corpora() {
  python3 -I - "$CORPUS" "$ATLAS_GROWTH" <<'PY'
import os, random, sys

out, atlas = sys.argv[1], sys.argv[2] == "1"
rng = random.Random(0x5EED)

# Operator T0 frame format: 256-color fg SGR, 256-color bg SGR, one emoji or
# ASCII character (the generator in frankenterm/term/benches/ingest/mod.rs).
emoji = [chr(c) for c in list(range(0x1F300, 0x1F5FF)) + list(range(0x1F900, 0x1F9FF))]
ascii_pool = [chr(c) for c in range(0x21, 0x7F) if chr(c) not in "$\\"]
pool = emoji + ascii_pool
with open(os.path.join(out, "10-color-emoji.txt"), "w") as handle:
    for _ in range(400_000):
        handle.write(f"\x1b[38;5;{rng.randrange(256)}m\x1b[48;5;{rng.randrange(256)}m{rng.choice(pool)}")
    handle.write("\x1b[0m\n")

with open(os.path.join(out, "20-seq-lines.txt"), "w") as handle:
    for n in range(1, 200_001):
        handle.write(f"{n}\n")

# In-pane scrolling: a scroll region with index / reverse index churn.
with open(os.path.join(out, "30-scroll-region.txt"), "w") as handle:
    handle.write("\x1b[2;20r")
    for n in range(5_000):
        handle.write(f"\x1b[20;1H\x1bD scroll line {n}\x1b[2;1H\x1bM")
    handle.write("\x1b[r\x1b[0m\n")

if atlas:
    # Distinct glyphs fill the atlas: CJK unified ideographs and Hangul
    # syllables in four faces, then double-height and double-width lines,
    # which rasterize the same glyphs again at another scale.
    chars = [chr(c) for c in list(range(0x4E00, 0xA000)) + list(range(0xAC00, 0xD7A4))]
    with open(os.path.join(out, "40-atlas-growth.txt"), "w") as handle:
        for face in ("", "\x1b[1m", "\x1b[3m", "\x1b[1;3m"):
            for start in range(0, len(chars), 60):
                handle.write(face + "".join(chars[start:start + 60]) + "\x1b[0m\n")
        for start in range(0, 4_000, 30):
            line = "".join(chars[start:start + 30])
            handle.write(f"\x1b#3{line}\n\x1b#4{line}\n\x1b#6{line}\n\x1b#5")
        handle.write("\x1b[0m\n")
PY
}

write_workload() {
  cat >"$RUN/workload.sh" <<'SH'
#!/bin/bash
# Runs inside the soak GUI's pane. Args: stop-file idle-seconds corpus-dir...
stop="$1"; idle="$2"; shift 2
passes=0
while [ ! -e "$stop" ]; do
  for dir in "$@"; do
    for file in "$dir"/*; do
      [ -e "$stop" ] && break 2
      [ -f "$file" ] || continue
      cat "$file"
    done
  done
  passes=$((passes + 1))
  printf '\033[0m\033[2J\033[Hsoak pass %d done\n' "$passes"
  sleep "$idle"
done
echo "soak workload stopped after $passes passes"
sleep 3600
SH
  chmod +x "$RUN/workload.sh"
}

isolation_probe() {
  local ft_bin
  ft_bin="$(command -v ft || true)"
  if [[ -z "$ft_bin" ]]; then
    log "isolation probe: ft not on PATH; relying on env -i + isolated HOME + --always-new-process"
    echo "skipped:no-ft"
    return 0
  fi
  local output status=0
  # Run from the run directory with its own workspace so the probe can only
  # see a live mux, never the repository's observed-pane store.
  mkdir -p "$RUN/ft-workspace"
  output="$(cd "$RUN" && "${ISO_ENV[@]}" "$ft_bin" list --json --workspace "$RUN/ft-workspace" 2>&1)" \
    || status=$?
  if [[ "$status" == 0 ]] && grep -q '"pane_id"' <<<"$output"; then
    log "isolation probe FAILED: ft under the isolated environment sees panes"
    echo "$output" >"$RUN/isolation-probe.txt"
    die "isolation probe found a live mux; refusing to launch"
  fi
  log "isolation probe: no mux visible under the isolated environment (ft exit $status)"
  echo "ok:exit-$status"
}

gui_alive() {
  local pid="$1"
  ps -o comm= -p "$pid" 2>/dev/null | grep -q .
}

resize_window() {
  local pid="$1" width="$2" height="$3"
  osascript -e "tell application \"System Events\" to set size of front window of (first process whose unix id is $pid) to {$width, $height}" 2>&1
}

stop_gui() {
  local pid="$1"
  [[ "$pid" =~ ^[0-9]+$ && "$pid" -gt 1 ]] || return 0
  touch "$STOP"
  sleep 2
  local cmd
  cmd="$(ps -o command= -p "$pid" 2>/dev/null || true)"
  if [[ "$cmd" != *"$GUI_BIN"* ]]; then
    log "not signalling pid $pid: it is no longer the soak GUI ($cmd)"
    return 0
  fi
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 20); do
    gui_alive "$pid" || return 0
    sleep 1
  done
  log "GUI pid $pid ignored SIGTERM for 20 s; sending SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
}

log "run directory $RUN"
log "gui $GUI_BIN; duration ${DURATION_S}s, bundle every ${SAMPLE_S}s, idle ${IDLE_S}s, resize every ${RESIZE_S}s"
generate_corpora
write_workload
CORPUS_DIRS=("$CORPUS")
if [[ -n "$CORPUS_DIR" ]]; then
  [[ -d "$CORPUS_DIR" ]] || die "--corpus-dir $CORPUS_DIR is not a directory"
  CORPUS_DIRS+=("$(cd "$CORPUS_DIR" && pwd)")
fi

if [[ "$DRY_RUN" == 1 ]]; then
  echo "plan: ${ISO_ENV[*]} $GUI_BIN start --always-new-process -- /bin/bash $RUN/workload.sh $STOP $IDLE_S ${CORPUS_DIRS[*]}"
  echo "corpora:"; ls -l "$CORPUS"
  exit 0
fi

PROBE="$(isolation_probe)"
LOAD_START="$(sysctl -n vm.loadavg)"
"${ISO_ENV[@]}" "$GUI_BIN" start --always-new-process -- \
  /bin/bash "$RUN/workload.sh" "$STOP" "$IDLE_S" "${CORPUS_DIRS[@]}" \
  >"$RUN/gui.stdout" 2>"$RUN/gui.stderr" &
GUI_PID=$!
trap 'stop_gui "$GUI_PID"' EXIT
log "launched GUI pid $GUI_PID"

for _ in $(seq 1 60); do
  [[ -f "$RUNTIME_DIR/frankenterm-resources-$GUI_PID.json" ]] && break
  gui_alive "$GUI_PID" || die "GUI exited during start-up (see $RUN/gui.stderr)"
  sleep 1
done
if [[ -f "$RUNTIME_DIR/frankenterm-resources-$GUI_PID.json" ]]; then
  log "GUI published its resource ledger"
else
  log "GUI published no resource ledger within 60 s; bundles will carry vmmap/footprint only"
fi

START_S="$(date +%s)"
NEXT_SAMPLE="$START_S"
NEXT_RESIZE=$(( START_S + RESIZE_S ))
RESIZES_OK=0
RESIZE_UNAVAILABLE=""
SAMPLES=0
GUI_DIED=0
SIZE_TOGGLE=0
while (( $(date +%s) - START_S < DURATION_S )); do
  if ! gui_alive "$GUI_PID"; then
    GUI_DIED=1
    log "GUI pid $GUI_PID exited before the run ended"
    break
  fi
  NOW="$(date +%s)"
  if (( NOW >= NEXT_SAMPLE )); then
    if "$FOOTPRINT" capture --pid "$GUI_PID" --out "$BUNDLES" --label soak \
      --runtime-dir "$RUNTIME_DIR" >>"$LOG" 2>&1; then
      SAMPLES=$((SAMPLES + 1))
    else
      log "footprint capture failed (see run.log)"
    fi
    NEXT_SAMPLE=$(( NOW + SAMPLE_S ))
  fi
  if [[ -z "$RESIZE_UNAVAILABLE" ]] && (( NOW >= NEXT_RESIZE )); then
    if (( SIZE_TOGGLE == 0 )); then size=(1100 700); else size=(1400 900); fi
    SIZE_TOGGLE=$(( 1 - SIZE_TOGGLE ))
    if result="$(resize_window "$GUI_PID" "${size[@]}")"; then
      RESIZES_OK=$((RESIZES_OK + 1))
    else
      RESIZE_UNAVAILABLE="$result"
      log "resize unavailable: $result"
    fi
    NEXT_RESIZE=$(( NOW + RESIZE_S ))
  fi
  sleep 1
done
LOAD_END="$(sysctl -n vm.loadavg)"
ELAPSED=$(( $(date +%s) - START_S ))

"$FOOTPRINT" growth "$BUNDLES" --gpu-budget-mib "$GPU_BUDGET_MIB" >"$RUN/growth.json" || true
ADVISORY=0
if (( DURATION_S < 3600 )); then
  ADVISORY=1
fi
stop_gui "$GUI_PID"
trap - EXIT

SOAK_GUI_BIN="$GUI_BIN" SOAK_RUN="$RUN" SOAK_DURATION_S="$DURATION_S" SOAK_ELAPSED_S="$ELAPSED" \
  SOAK_SAMPLE_S="$SAMPLE_S" SOAK_SAMPLES="$SAMPLES" SOAK_RESIZES_OK="$RESIZES_OK" \
  SOAK_RESIZE_UNAVAILABLE="$RESIZE_UNAVAILABLE" SOAK_PROBE="$PROBE" SOAK_GUI_DIED="$GUI_DIED" \
  SOAK_LOAD_START="$LOAD_START" SOAK_LOAD_END="$LOAD_END" SOAK_GPU_BUDGET_MIB="$GPU_BUDGET_MIB" \
  SOAK_ADVISORY="$ADVISORY" \
  python3 -I - "$RUN/growth.json" "$RUN/receipt.json" <<'PY'
import json, os, sys

env = os.environ
try:
    with open(sys.argv[1]) as handle:
        verdict = json.loads(handle.read() or "{}")
except ValueError as error:
    verdict = {"pass": False, "failures": [f"growth verdict unreadable: {error}"]}
advisory = env["SOAK_ADVISORY"] == "1"
receipt = {
    "schema": "frankenterm.gui_soak_receipt.v1",
    "gui_bin": env["SOAK_GUI_BIN"],
    "run_dir": env["SOAK_RUN"],
    "requested_duration_s": int(env["SOAK_DURATION_S"]),
    "elapsed_s": int(env["SOAK_ELAPSED_S"]),
    "sample_interval_s": int(env["SOAK_SAMPLE_S"]),
    "bundles_captured": int(env["SOAK_SAMPLES"]),
    "resizes_ok": int(env["SOAK_RESIZES_OK"]),
    "resize_unavailable": env["SOAK_RESIZE_UNAVAILABLE"] or None,
    "isolation_probe": env["SOAK_PROBE"],
    "gui_exited_early": env["SOAK_GUI_DIED"] == "1",
    "host_load_average_start": env["SOAK_LOAD_START"],
    "host_load_average_end": env["SOAK_LOAD_END"],
    "gpu_budget_mib": int(env["SOAK_GPU_BUDGET_MIB"]),
    "growth_verdict": verdict,
    "growth_verdict_advisory": advisory,
}
receipt["pass"] = (not receipt["gui_exited_early"]) and receipt["bundles_captured"] >= 4 and (
    advisory or verdict.get("pass", False))
with open(sys.argv[2], "w") as handle:
    json.dump(receipt, handle, indent=2, sort_keys=True)
print(json.dumps({"pass": receipt["pass"], "receipt": sys.argv[2]}))
sys.exit(0 if receipt["pass"] else 1)
PY
