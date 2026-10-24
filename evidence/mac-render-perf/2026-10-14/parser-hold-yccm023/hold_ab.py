#!/usr/bin/python3 -I
"""ft-yccm0.2.3 AC3 native check: parser lock holds while a pane floods T0 output.

One GUI, one small window brought in front of other apps' windows (the app
stays inactive), whose pane runs `cat CORPUS` in a loop. After SECS seconds
the GUI's resource snapshot gives the parser's terminal lock holds and the
paint's waits (terminal_locks, cumulative since launch) and the frames it
presented. The GUI runs under its own throwaway HOME with an absolute
--config-file, launched with `open -g -n`, and is stopped by the exact pid
found by that HOME. The run directory is kept.

usage: hold_ab.py APP CORPUS [label] [secs]
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time

APP, CORPUS = sys.argv[1], os.path.abspath(sys.argv[2])
LABEL = sys.argv[3] if len(sys.argv) > 3 else os.path.basename(os.path.dirname(APP))
SECS = float(sys.argv[4]) if len(sys.argv) > 4 else 20
LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_HA_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
c.font_size = 10
c.initial_cols = tonumber(os.getenv('FT_HA_COLS') or '') or 80
c.initial_rows = tonumber(os.getenv('FT_HA_ROWS') or '') or 12
c.window_decorations = 'RESIZE|MACOS_FORCE_DISABLE_SHADOW'
ft.on('gui-startup', function()
  local _, _, window = ft.mux.spawn_window {
    args = { '/bin/sh', '-c', 'while :; do /bin/cat "$FT_HA_CORPUS"; done' },
  }
  -- Pixels from the top left; FT_CV_X/FT_CV_Y override (cover_park.py).
  local x = tonumber(os.getenv('FT_CV_X') or '') or 80
  local y = tonumber(os.getenv('FT_CV_Y') or '') or 756
  ft.time.call_after(1, function()
    window:gui_window():set_position(x, y)
    window:gui_window():focus()
  end)
end)
return c
"""
D = tempfile.mkdtemp(prefix='ftha.', dir='/tmp')
CONF = os.path.join(D, 'c.lua')
with open(CONF, 'w') as f:
    f.write(LUA)


def gui_pid(home):
    for p in subprocess.run(['pgrep', '-f', 'Contents/MacOS/frankenterm-gui'],
                            capture_output=True, text=True).stdout.split():
        cmd = subprocess.run(['ps', '-E', '-ww', '-p', p, '-o', 'command='],
                             capture_output=True, text=True).stdout
        if re.search(r'(^| )HOME=' + re.escape(home) + r'( |$)', cmd):
            return int(p)
    return None


def snapshot(home, pid):
    try:
        with open(f'{home}/.local/share/frankenterm/frankenterm-resources-{pid}.json') as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def stop(pid):
    os.kill(pid, signal.SIGTERM)
    for _ in range(40):
        try:
            os.kill(pid, 0)
        except OSError:
            return
        time.sleep(0.25)
    os.kill(pid, signal.SIGKILL)


def ms(ns):
    return round(ns / 1e6, 3)


home = os.path.join(D, 'home')
os.makedirs(home)
subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_HA_HOME={home}',
                '--env', 'FRANKENTERM_LUA_CONFIG=1', '--env', f'FT_HA_CORPUS={CORPUS}',
                '--env', 'FT_RESOURCE_SNAPSHOT_INTERVAL_MS=500',
                '--env', f'FT_CV_X={os.environ.get("FT_CV_X", "")}',
                '--env', f'FT_CV_Y={os.environ.get("FT_CV_Y", "")}',
                '--env', f'FT_HA_COLS={os.environ.get("FT_HA_COLS", "")}',
                '--env', f'FT_HA_ROWS={os.environ.get("FT_HA_ROWS", "")}',
                '--env', f'RUST_LOG={os.environ.get("RUST_LOG", "warn")}',
                '--stderr', f'{home}/err', '-a', APP, '--args', '--config-file', CONF, 'start'],
               check=True)
pid = None
for _ in range(80):
    pid = gui_pid(home)
    if pid and snapshot(home, pid) is not None:
        break
    time.sleep(0.25)
else:
    raise SystemExit('the GUI did not start')
try:
    time.sleep(3)
    a, ta = snapshot(home, pid), time.monotonic()
    time.sleep(SECS)
    b, tb = snapshot(home, pid), time.monotonic()
finally:
    stop(pid)
holders = b['terminal_locks']['holders']
hold, wait = holders['parser']['hold'], holders['paint']['wait']
fps = (b['frames']['presented_total'] - a['frames']['presented_total']) / (tb - ta)
print(f'{LABEL}: parser holds {hold["count"]}: p50 {ms(hold["p50_ns"])} p95 {ms(hold["p95_ns"])} '
      f'p99 {ms(hold["p99_ns"])} max {ms(hold["max_ns"])} ms (mean {ms(hold["total_ns"] / max(1, hold["count"]))}); '
      f'paint wait p99 {ms(wait["p99_ns"])} max {ms(wait["max_ns"])} ms; '
      f'{fps:.1f} frames/s presented', flush=True)
with open(os.path.join(D, 'snapshot.json'), 'w') as f:
    json.dump(b, f)
# Never deletes (AGENTS.md RULE 1).
print('run directory kept:', D)
