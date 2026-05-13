#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/wezterm-render-comparison-macos.sh [options]

Export a real FrankenTerm-vs-upstream-WezTerm PNG frame comparison report on
macOS. The adapter launches each GUI with a deterministic borderless config,
plays the fixed terminal-conformance transcripts into the pane, captures the
window content, and asks the GPU regression harness to write
schema_version=wezterm-render-comparison.v1.

Options:
  --frankenterm-gui <path>   FrankenTerm GUI binary (default: target/debug/frankenterm-gui).
  --wezterm-gui <path>       Upstream WezTerm GUI binary (required unless --self-test).
  --manifest <path>          Terminal-conformance manifest.
  --frame-root <path>        Directory for captured frames.
  --output <path>            Comparison report path.
  --timeout-secs <seconds>   Per-window readiness timeout (default: 20).
  --self-test                Exercise comparison-report generation with fixture PNGs only.

Environment:
  FT_WEZTERM_FRAME_RECT       Capture rectangle as x,y,w,h. Defaults to 80,80,960,480.
                              The GUI is launched at 80,80 with no decorations, so this
                              avoids macOS accessibility APIs on GitHub-hosted runners.
  FT_WEZTERM_FRAME_SETTLE_SECS Seconds to wait after transcript readiness before capture.
                              Defaults to 3.
  FT_WEZTERM_FRAME_CAPTURE_TIMEOUT_SECS
                              Seconds to allow screencapture to complete.
                              Defaults to 15.
  FT_WEZTERM_FRAME_EXIT_SECS  Seconds to wait for GUI exit after capture before cleanup.
                              Defaults to 10.
  FT_WEZTERM_FRAME_CLEANUP_SETTLE_SECS
                              Seconds to wait after GUI cleanup before the next capture.
                              Defaults to 2.
  -h, --help                 Show this help.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"

FRANKENTERM_GUI="$PROJECT_ROOT/target/debug/frankenterm-gui"
WEZTERM_GUI=""
MANIFEST="$PROJECT_ROOT/tests/fixtures/terminal-conformance/manifest.json"
FRAME_ROOT="$PROJECT_ROOT/target/wezterm-differential/$RUN_ID/frames"
OUTPUT="$PROJECT_ROOT/target/wezterm-differential/$RUN_ID/comparison-report.json"
TIMEOUT_SECS=20
SELF_TEST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --frankenterm-gui)
      FRANKENTERM_GUI="${2:?--frankenterm-gui requires a path}"
      shift 2
      ;;
    --wezterm-gui)
      WEZTERM_GUI="${2:?--wezterm-gui requires a path}"
      shift 2
      ;;
    --manifest)
      MANIFEST="${2:?--manifest requires a path}"
      shift 2
      ;;
    --frame-root)
      FRAME_ROOT="${2:?--frame-root requires a path}"
      shift 2
      ;;
    --output)
      OUTPUT="${2:?--output requires a path}"
      shift 2
      ;;
    --timeout-secs)
      TIMEOUT_SECS="${2:?--timeout-secs requires a value}"
      shift 2
      ;;
    --self-test)
      SELF_TEST=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[wezterm-render-adapter] unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

require_tool() {
  local name="$1"
  if ! command -v "$name" >/dev/null 2>&1; then
    echo "[wezterm-render-adapter] missing required tool: $name" >&2
    exit 69
  fi
}


abspath_parented() {
  local path="$1"
  local dir
  local base
  dir="$(dirname "$path")"
  base="$(basename "$path")"
  if [[ ! -d "$dir" ]]; then
    echo "[wezterm-render-adapter] path parent does not exist: $dir" >&2
    exit 66
  fi
  dir="$(cd "$dir" && pwd -P)"
  printf "%s/%s\n" "$dir" "$base"
}

canonicalize_report_paths() {
  mkdir -p "$FRAME_ROOT" "$(dirname "$OUTPUT")"
  FRAME_ROOT="$(abspath_parented "$FRAME_ROOT")"
  OUTPUT="$(abspath_parented "$OUTPUT")"
  MANIFEST="$(abspath_parented "$MANIFEST")"
}

canonicalize_gui_paths() {
  FRANKENTERM_GUI="$(abspath_parented "$FRANKENTERM_GUI")"
  WEZTERM_GUI="$(abspath_parented "$WEZTERM_GUI")"
}

write_comparison_report() {
  local frankenterm_frames="$1"
  local wezterm_frames="$2"
  local output="$3"

  cargo test -p frankenterm-gui --test gpu_regression -- \
    --comparison-report="$output" \
    --comparison-frankenterm-frames="$frankenterm_frames" \
    --comparison-wezterm-frames="$wezterm_frames"
}

run_self_test() {
  local self_root="$FRAME_ROOT/self-test"
  local ft_root="$self_root/frankenterm"
  local wz_root="$self_root/wezterm"
  local fixture="$PROJECT_ROOT/tests/golden/gpu/_smoketest/golden.png"

  mkdir -p "$ft_root/self-test" "$wz_root/self-test"
  cp "$fixture" "$ft_root/self-test/frame-000.png"
  cp "$fixture" "$wz_root/self-test/frame-000.png"
  write_comparison_report "$ft_root" "$wz_root" "$OUTPUT"
  echo "[wezterm-render-adapter] self-test PASS output=$OUTPUT"
}

lua_string_array() {
  python3 - "$@" <<'PY'
import json
import sys

print("{ " + ", ".join(json.dumps(arg) for arg in sys.argv[1:]) + " }")
PY
}

write_config() {
  local path="$1"
  shift
  local default_prog
  default_prog="$(lua_string_array "$@")"

  cat >"$path" <<LUA
local wezterm = require 'wezterm'

return {
  automatically_reload_config = false,
  check_for_updates = false,
  enable_tab_bar = false,
  hide_tab_bar_if_only_one_tab = true,
  window_decorations = "NONE",
  window_close_confirmation = "NeverPrompt",
  exit_behavior = "Close",
  initial_cols = 80,
  initial_rows = 24,
  font_size = 12.0,
  -- Use a stock macOS font so absent-font config overlays do not contaminate screenshots.
  font = wezterm.font("Menlo"),
  window_padding = { left = 0, right = 0, top = 0, bottom = 0 },
  cursor_blink_rate = 0,
  audible_bell = "Disabled",
  animation_fps = 1,
  max_fps = 60,
  front_end = "WebGpu",
  default_prog = $default_prog,
  colors = {
    background = "#000000",
    foreground = "#ffffff",
    cursor_bg = "#ffffff",
    cursor_border = "#ffffff",
  },
}
LUA
}

write_driver() {
  local path="$1"
  cat >"$path" <<'PY'
#!/usr/bin/env python3
import os
import pathlib
import sys
import time

hex_path = pathlib.Path(sys.argv[1])
title = sys.argv[2]
ready_path = pathlib.Path(sys.argv[3])
hold_secs = float(os.environ.get("FT_WEZTERM_FRAME_HOLD_SECS", "8"))

raw_hex = "".join(hex_path.read_text(encoding="utf-8").split())
payload = bytes.fromhex(raw_hex)
stdout = sys.stdout.buffer
stdout.write(b"\x1b]2;" + title.encode("utf-8") + b"\x07")
stdout.write(payload)
stdout.flush()
ready_path.write_text("ready\n", encoding="utf-8")
time.sleep(hold_secs)
PY
  chmod +x "$path"
}

manifest_rows() {
  python3 - "$MANIFEST" <<'PY'
import json
import pathlib
import sys

manifest = pathlib.Path(sys.argv[1])
root = manifest.parent.resolve()
data = json.loads(manifest.read_text(encoding="utf-8"))
for scenario in data.get("scenarios", []):
    scenario_id = scenario["scenario_id"]
    transcript = (root / scenario["input_artifact"]).resolve()
    print(f"{scenario_id}\t{transcript}")
PY
}

wait_for_file() {
  local path="$1"
  local timeout_secs="$2"
  local status_file="${3:-}"
  local log_file="${4:-}"
  local waited=0
  while [[ ! -f "$path" ]]; do
    if [[ -n "$status_file" && -f "$status_file" ]]; then
      local status
      status="$(cat "$status_file")"
      echo "[wezterm-render-adapter] GUI exited with status $status before $path was ready" >&2
      if [[ -n "$log_file" && -s "$log_file" ]]; then
        echo "[wezterm-render-adapter] GUI log excerpt from $log_file:" >&2
        sed -n '1,160p' "$log_file" >&2
      fi
      exit 75
    fi
    if [[ "$waited" -ge "$timeout_secs" ]]; then
      echo "[wezterm-render-adapter] timed out waiting for $path" >&2
      if [[ -n "$log_file" && -s "$log_file" ]]; then
        echo "[wezterm-render-adapter] GUI log excerpt from $log_file:" >&2
        sed -n '1,160p' "$log_file" >&2
      fi
      exit 75
    fi
    sleep 1
    waited=$((waited + 1))
  done
}

run_with_timeout() {
  local label="$1"
  local timeout_secs="$2"
  shift 2

  if ! [[ "$timeout_secs" =~ ^[0-9]+$ ]] || [[ "$timeout_secs" -le 0 ]]; then
    echo "[wezterm-render-adapter] invalid timeout for $label: $timeout_secs; expected positive seconds" >&2
    exit 75
  fi

  "$@" &
  local command_pid=$!
  (
    sleep "$timeout_secs"
    if kill -0 "$command_pid" >/dev/null 2>&1; then
      echo "[wezterm-render-adapter] timed out after ${timeout_secs}s: $label" >&2
      kill -TERM "$command_pid" >/dev/null 2>&1 || true
      sleep 2
      if kill -0 "$command_pid" >/dev/null 2>&1; then
        kill -KILL "$command_pid" >/dev/null 2>&1 || true
      fi
    fi
  ) &
  local watchdog_pid=$!
  local status=0

  wait "$command_pid" || status=$?
  kill "$watchdog_pid" >/dev/null 2>&1 || true
  wait "$watchdog_pid" >/dev/null 2>&1 || true

  if [[ "$status" -eq 143 || "$status" -eq 137 ]]; then
    return 124
  fi
  return "$status"
}

capture_window() {
  local title="$1"
  local output="$2"
  local bounds="${FT_WEZTERM_FRAME_RECT:-80,80,960,480}"
  local capture_timeout_secs="${FT_WEZTERM_FRAME_CAPTURE_TIMEOUT_SECS:-15}"

  if [[ ! "$bounds" =~ ^[0-9]+,[0-9]+,[0-9]+,[0-9]+$ ]]; then
    echo "[wezterm-render-adapter] invalid FT_WEZTERM_FRAME_RECT=$bounds; expected x,y,w,h" >&2
    exit 75
  fi
  mkdir -p "$(dirname "$output")"
  echo "[wezterm-render-adapter] capture title=$title rect=$bounds output=$output"
  local status=0
  run_with_timeout "screencapture title=$title" "$capture_timeout_secs" \
    screencapture -x -R "$bounds" "$output" || status=$?
  if [[ "$status" -ne 0 ]]; then
    echo "[wezterm-render-adapter] screencapture failed title=$title status=$status" >&2
    exit 75
  fi
}


collect_process_tree_pids() {
  local root="$1"
  local child

  while IFS= read -r child; do
    [[ -n "$child" ]] || continue
    collect_process_tree_pids "$child"
  done < <(pgrep -P "$root" 2>/dev/null || true)

  printf "%s\n" "$root"
}

collect_gui_cleanup_pids() {
  local gui_pid="$1"
  local class="$2"

  {
    collect_process_tree_pids "$gui_pid"
    pgrep -f "$class" 2>/dev/null || true
  } | sort -un
}

terminate_gui_cleanup_pids() {
  local signal="$1"
  local cleanup_pids="$2"
  local pid

  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    kill "-$signal" "$pid" >/dev/null 2>&1 || true
  done <<<"$cleanup_pids"
}

any_gui_cleanup_pid_alive() {
  local cleanup_pids="$1"
  local pid

  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    if kill -0 "$pid" >/dev/null 2>&1; then
      return 0
    fi
  done <<<"$cleanup_pids"

  return 1
}

cleanup_lingering_gui_processes() {
  local gui_pid="$1"
  local class="$2"
  local cleanup_pids
  local cleanup_pid_list

  cleanup_pids="$(collect_gui_cleanup_pids "$gui_pid" "$class")"
  [[ -n "$cleanup_pids" ]] || return 0

  cleanup_pid_list="$(printf "%s\n" "$cleanup_pids" | tr "\n" " ")"
  echo "[wezterm-render-adapter] cleaning GUI process tree class=$class pids=$cleanup_pid_list" >&2
  terminate_gui_cleanup_pids TERM "$cleanup_pids"
  sleep 2
  if any_gui_cleanup_pid_alive "$cleanup_pids"; then
    terminate_gui_cleanup_pids KILL "$cleanup_pids"
  fi
}

settle_after_gui_cleanup() {
  sleep "${FT_WEZTERM_FRAME_CLEANUP_SETTLE_SECS:-2}"
}
wait_or_terminate_after_capture() {
  local engine="$1"
  local scenario_id="$2"
  local gui_pid="$3"
  local log_file="$4"
  local class="$5"
  local timeout_secs="${FT_WEZTERM_FRAME_EXIT_SECS:-10}"
  local waited=0

  if ! [[ "$timeout_secs" =~ ^[0-9]+$ ]]; then
    echo "[wezterm-render-adapter] invalid FT_WEZTERM_FRAME_EXIT_SECS=$timeout_secs; expected seconds" >&2
    exit 75
  fi

  while kill -0 "$gui_pid" >/dev/null 2>&1; do
    if [[ "$waited" -ge "$timeout_secs" ]]; then
      echo "[wezterm-render-adapter] terminating GUI after capture engine=$engine scenario=$scenario_id pid=$gui_pid class=$class" >&2
      cleanup_lingering_gui_processes "$gui_pid" "$class"
      wait "$gui_pid" >/dev/null 2>&1 || true
      settle_after_gui_cleanup
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done

  if ! wait "$gui_pid"; then
    echo "[wezterm-render-adapter] GUI exited non-zero after capture for engine=$engine scenario=$scenario_id" >&2
    if [[ -s "$log_file" ]]; then
      echo "[wezterm-render-adapter] GUI log excerpt from $log_file:" >&2
      sed -n '1,160p' "$log_file" >&2
    fi
    exit 75
  fi

  cleanup_lingering_gui_processes "$gui_pid" "$class"
  settle_after_gui_cleanup
}


validate_frame_capture_fingerprints() {
  local engine="$1"
  local frame_dir="$2"

  python3 - "$engine" "$frame_dir" <<\PY
import hashlib
import pathlib
import sys

engine = sys.argv[1]
root = pathlib.Path(sys.argv[2])
frames = sorted(root.glob("*/frame-*.png"))
if not frames:
    raise SystemExit(f"[wezterm-render-adapter] no captured frames for {engine} under {root}")

by_hash = {}
for frame in frames:
    digest = hashlib.sha256(frame.read_bytes()).hexdigest()
    by_hash.setdefault(digest, []).append(str(frame.relative_to(root)))

if len(frames) > 1 and len(by_hash) == 1:
    examples = ", ".join(next(iter(by_hash.values()))[:5])
    raise SystemExit(
        f"[wezterm-render-adapter] stale capture suspect: all {len(frames)} "
        f"{engine} frames are byte-identical ({examples})"
    )

print(
    f"[wezterm-render-adapter] frame fingerprints engine={engine} "
    f"frames={len(frames)} unique_hashes={len(by_hash)}"
)
PY
}

export_engine_frames() {
  local engine="$1"
  local gui="$2"
  local frame_dir="$3"
  local run_dir="$FRAME_ROOT/run-$engine"
  local driver="$run_dir/render_transcript.py"

  if [[ ! -x "$gui" ]]; then
    echo "[wezterm-render-adapter] $engine GUI binary is not executable: $gui" >&2
    exit 66
  fi

  mkdir -p "$run_dir" "$frame_dir"
  run_dir="$(cd "$run_dir" && pwd -P)"
  frame_dir="$(cd "$frame_dir" && pwd -P)"
  driver="$run_dir/render_transcript.py"
  write_driver "$driver"

  while IFS=$'\t' read -r scenario_id transcript; do
    local title="ft-wezterm-diff-$engine-$scenario_id-$RUN_ID"
    local config="$run_dir/$scenario_id.lua"
    local ready="$run_dir/$scenario_id.ready"
    local output="$frame_dir/$scenario_id/frame-000.png"
    local class="ft-wezterm-diff-$engine-$scenario_id-$RUN_ID"
    local log_file="$run_dir/$scenario_id.log"
    local -a launch_env=()

    if [[ "$engine" == "frankenterm" ]]; then
      # FrankenTerm defaults to TOML config and only accepts generated Lua
      # config files when explicitly enabled. The upstream WezTerm side still
      # requires Lua, so keep the generated file format and opt FrankenTerm in.
      launch_env+=(FRANKENTERM_LUA_CONFIG=1 FT_MACOS_BACKEND=wgpu)
    fi

    write_config "$config" python3 "$driver" "$transcript" "$title" "$ready"
    echo "[wezterm-render-adapter] export engine=$engine scenario=$scenario_id"
    if ((${#launch_env[@]} > 0)); then
      env "${launch_env[@]}" "$gui" --config-file "$config" start \
        --always-new-process \
        --class "$class" \
        --position 80,80 >"$log_file" 2>&1 &
    else
      "$gui" --config-file "$config" start \
        --always-new-process \
        --class "$class" \
        --position 80,80 >"$log_file" 2>&1 &
    fi
    local gui_pid=$!
    wait_for_file "$ready" "$TIMEOUT_SECS" "" "$log_file"
    sleep "${FT_WEZTERM_FRAME_SETTLE_SECS:-3}"
    capture_window "$title" "$output"
    wait_or_terminate_after_capture "$engine" "$scenario_id" "$gui_pid" "$log_file" "$class"
  done < <(manifest_rows)
}

if [[ "$SELF_TEST" -eq 1 ]]; then
  require_tool cargo
  canonicalize_report_paths
  run_self_test
  exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "[wezterm-render-adapter] real frame export requires macOS" >&2
  exit 69
fi
if [[ -z "$WEZTERM_GUI" ]]; then
  echo "[wezterm-render-adapter] --wezterm-gui is required" >&2
  exit 64
fi

require_tool cargo
require_tool python3
require_tool screencapture
require_tool pgrep
canonicalize_report_paths
canonicalize_gui_paths

FRANKENTERM_FRAME_ROOT="$FRAME_ROOT/frankenterm"
WEZTERM_FRAME_ROOT="$FRAME_ROOT/wezterm"

export_engine_frames "frankenterm" "$FRANKENTERM_GUI" "$FRANKENTERM_FRAME_ROOT"
validate_frame_capture_fingerprints "frankenterm" "$FRANKENTERM_FRAME_ROOT"
export_engine_frames "wezterm" "$WEZTERM_GUI" "$WEZTERM_FRAME_ROOT"
validate_frame_capture_fingerprints "wezterm" "$WEZTERM_FRAME_ROOT"
write_comparison_report "$FRANKENTERM_FRAME_ROOT" "$WEZTERM_FRAME_ROOT" "$OUTPUT"

echo "[wezterm-render-adapter] output=$OUTPUT"
