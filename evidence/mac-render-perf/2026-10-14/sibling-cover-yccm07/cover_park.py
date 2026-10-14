#!/usr/bin/python3 -I
"""ft-yccm0.7 native check: a window covered by another FrankenTerm window parks.

One GUI opens three small windows that print continuously, stacked at the
same position in a free corner of the main screen, opaque (opacity 1.0 with
an opaque gradient layer), titled, no shadow: the operator's back windows in
miniature. The GUI's own timers then change the stack:
  0-22 s   all three stacked: the two behind the front one are covered;
  22-42 s  the front window moves 400 px right: the middle one shows, the
           back one stays covered;
  42-62 s  the middle window turns glass (opacity 0.9): nothing is covered.
Each phase reports the GUI's presented frame rate, its swapchain drawables
(resource snapshot) and the window layer's cover transitions (debug log).

The GUI runs under its own throwaway HOME with an absolute --config-file,
launched with `open -g -n` (it never takes focus), and is stopped by the exact
pid found by that HOME. The run directory is kept.

usage: cover_park.py APP [label]
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time

APP = sys.argv[1]
LABEL = sys.argv[2] if len(sys.argv) > 2 else os.path.basename(os.path.dirname(APP))
LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_CV_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
c.font_size = 10
c.initial_cols = 40
c.initial_rows = 8
c.window_decorations = 'RESIZE|MACOS_FORCE_DISABLE_SHADOW'
c.window_background_opacity = 1.0
c.background = { {
  source = { Gradient = { colors = { '#060a10', '#0a1018', '#0e1420' },
                          orientation = { Linear = { angle = -45.0 } } } },
  width = '100%', height = '100%', opacity = 1.0,
} }
local printing = { '/usr/bin/perl', '-e',
  '$|=1; my $i=0; while(1){ print $i++, " ", "x" x 30, "\n"; select(undef,undef,undef,0.004) }' }
ft.on('gui-startup', function()
  -- Pixels from the top left of the main screen. The default is the corner
  -- above the Dock of mac-mini-old (1683x668 pt at 2x), where nothing else
  -- is on screen; FT_CV_X/FT_CV_Y override it.
  local x = tonumber(os.getenv('FT_CV_X') or '') or 80
  local y = tonumber(os.getenv('FT_CV_Y') or '') or 756
  local wins = {}
  for i = 1, 3 do
    local _, _, window = ft.mux.spawn_window { args = printing }
    wins[i] = window
  end
  -- Bring them forward in order, so window 3 is in front and all three are
  -- above other apps' windows (the app stays inactive: no keyboard focus).
  ft.time.call_after(1, function()
    for i = 1, 3 do
      wins[i]:gui_window():set_position(x, y)
      wins[i]:gui_window():focus()
    end
  end)
  ft.time.call_after(22, function() wins[3]:gui_window():set_position(x + 400, y) end)
  ft.time.call_after(42, function()
    wins[2]:gui_window():set_config_overrides { window_background_opacity = 0.9 }
  end)
end)
return c
"""
D = tempfile.mkdtemp(prefix='ftcv.', dir='/tmp')
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
            d = json.load(f)
        drawable = d['gpu']['textures']['drawable']
        return d['frames']['presented_total'], drawable['live_count'], drawable['live_bytes']
    except (OSError, ValueError, KeyError):
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


home = os.path.join(D, 'home')
os.makedirs(home)
subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_CV_HOME={home}',
                '--env', 'FRANKENTERM_LUA_CONFIG=1', '--env', 'RUST_LOG=window=debug,warn',
                '--env', 'FT_RESOURCE_SNAPSHOT_INTERVAL_MS=500',
                '--env', f'FT_CV_X={os.environ.get("FT_CV_X", "")}',
                '--env', f'FT_CV_Y={os.environ.get("FT_CV_Y", "")}',
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
t0 = time.monotonic()
print(f'{LABEL}: pid {pid}, run dir {D}', flush=True)


def phase(name, start, end):
    time.sleep(max(0, t0 + start - time.monotonic()))
    a = snapshot(home, pid)
    ta = time.monotonic()
    time.sleep(max(0, t0 + end - time.monotonic()))
    b = snapshot(home, pid)
    tb = time.monotonic()
    fps = round((b[0] - a[0]) / (tb - ta), 1) if a and b else None
    print(f'{LABEL}: {name}: {fps} frames/s presented, drawables {b[1]} '
          f'({b[2] / 1048576:.1f} MiB)', flush=True)


try:
    phase('three stacked (12-21 s)', 12, 21)
    phase('front moved aside (32-41 s)', 32, 41)
    phase('middle turned glass (52-61 s)', 52, 61)
finally:
    stop(pid)
    time.sleep(0.5)
    with open(f'{home}/err', errors='replace') as f:
        covers = [line.split('window::os::macos::window]', 1)[-1].strip()
                  for line in f if 'covered by another window' in line]
    print(f'{LABEL}: cover transitions: {covers}')
    # Never deletes (AGENTS.md RULE 1).
    print('run directory kept:', D)
