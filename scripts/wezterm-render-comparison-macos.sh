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
  font = wezterm.font_with_fallback({ "Pragmasevka", "Menlo", "Monaco" }),
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

capture_window() {
  local title="$1"
  local output="$2"
  local bounds="${FT_WEZTERM_FRAME_RECT:-80,80,960,480}"

  if [[ ! "$bounds" =~ ^[0-9]+,[0-9]+,[0-9]+,[0-9]+$ ]]; then
    echo "[wezterm-render-adapter] invalid FT_WEZTERM_FRAME_RECT=$bounds; expected x,y,w,h" >&2
    exit 75
  fi
  mkdir -p "$(dirname "$output")"
  echo "[wezterm-render-adapter] capture title=$title rect=$bounds output=$output"
  screencapture -x -R "$bounds" "$output"
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
    local class="ft-wezterm-diff-$engine"
    local status_file="$run_dir/$scenario_id.status"
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
    (
      set +e
      env "${launch_env[@]}" "$gui" --config-file "$config" start \
        --always-new-process \
        --class "$class" \
        --position 80,80 >"$log_file" 2>&1
      status=$?
      printf '%s\n' "$status" >"$status_file"
      exit "$status"
    ) &
    local gui_pid=$!
    wait_for_file "$ready" "$TIMEOUT_SECS" "$status_file" "$log_file"
    sleep "${FT_WEZTERM_FRAME_SETTLE_SECS:-1}"
    capture_window "$title" "$output"
    if ! wait "$gui_pid"; then
      echo "[wezterm-render-adapter] GUI exited non-zero after capture for engine=$engine scenario=$scenario_id" >&2
      if [[ -s "$log_file" ]]; then
        echo "[wezterm-render-adapter] GUI log excerpt from $log_file:" >&2
        sed -n '1,160p' "$log_file" >&2
      fi
      exit 75
    fi
  done < <(manifest_rows)
}

if [[ "$SELF_TEST" -eq 1 ]]; then
  require_tool cargo
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

FRANKENTERM_FRAME_ROOT="$FRAME_ROOT/frankenterm"
WEZTERM_FRAME_ROOT="$FRAME_ROOT/wezterm"

export_engine_frames "frankenterm" "$FRANKENTERM_GUI" "$FRANKENTERM_FRAME_ROOT"
export_engine_frames "wezterm" "$WEZTERM_GUI" "$WEZTERM_FRAME_ROOT"
write_comparison_report "$FRANKENTERM_FRAME_ROOT" "$WEZTERM_FRAME_ROOT" "$OUTPUT"

echo "[wezterm-render-adapter] output=$OUTPUT"
