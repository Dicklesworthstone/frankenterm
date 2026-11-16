#!/usr/bin/python3 -I
"""Does a covered FrankenTerm window keep its swapchain drawables?

GUI A (maximized, printing) is measured with `footprint` while visible, then
it hides its own app (the Hide action) and is measured again, at once and
after 60 s. Each GUI runs under its own throwaway HOME with an absolute
--config-file, launched with `open -g -n`, stopped by the exact pid found by
that HOME.

usage: occl_footprint.py APP
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
LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_OC_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
local printing = { '/usr/bin/perl', '-e',
  '$|=1; my $i=0; while(1){ print $i++, " ", "x" x 100, "\n"; select(undef,undef,undef,0.004) }' }
local idle = { '/bin/sh', '-c', 'printf idle; exec sleep 100000' }
ft.on('gui-startup', function()
  local args = os.getenv('FT_OC_MODE') == 'print' and printing or idle
  local _, pane, window = ft.mux.spawn_window { args = args }
  ft.time.call_after(1, function() window:gui_window():maximize() end)
  local hide = tonumber(os.getenv('FT_OC_HIDE_AFTER') or '')
  if hide then
    ft.time.call_after(hide, function() window:gui_window():perform_action(ft.action.Hide, pane) end)
  end
end)
return c
"""
D = tempfile.mkdtemp(prefix='ftoc.', dir='/tmp')
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


def frames(home, pid):
    try:
        with open(f'{home}/.local/share/frankenterm/frankenterm-resources-{pid}.json') as f:
            return json.load(f)['frames']['presented_total']
    except (OSError, ValueError, KeyError):
        return None


def launch(name, mode, hide_after=None):
    home = os.path.join(D, name)
    os.makedirs(home)
    subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_OC_HOME={home}',
                    '--env', f'FT_OC_MODE={mode}', '--env', 'FRANKENTERM_LUA_CONFIG=1',
                    '--env', f'FT_OC_HIDE_AFTER={hide_after or ""}',
                    '--stderr', f'{home}/err', '-a', APP, '--args', '--config-file', CONF, 'start'],
                   check=True)
    for _ in range(80):
        pid = gui_pid(home)
        if pid and frames(home, pid) is not None:
            return home, pid
        time.sleep(0.25)
    raise SystemExit(f'{name} did not start')


def surfaces(pid):
    """Dirty MiB of the GPU-backed footprint categories, and the total."""
    out = subprocess.run(['footprint', '-p', str(pid)], capture_output=True, text=True).stdout
    unit = {'B': 1 / 1048576, 'KB': 1 / 1024, 'MB': 1, 'GB': 1024}
    rows = {}
    for line in out.splitlines():
        m = re.match(r'\s*([\d.]+)\s*(B|KB|MB|GB)\s+[\d.]+\s*(?:B|KB|MB|GB)\s+[\d.]+\s*(?:B|KB|MB|GB)\s+\d+\s+(.*?)\s*$', line)
        if m and ('IOSurface' in m.group(3) or 'graphics' in m.group(3)):
            rows[m.group(3)] = round(float(m.group(1)) * unit[m.group(2)], 1)
    total = re.search(r'Footprint:\s*([\d.]+)\s*(KB|MB|GB)', out)
    if total:
        rows['footprint_mib'] = round(float(total.group(1)) * unit[total.group(2)], 1)
    return rows


def fps(home, pid, seconds=5):
    f0, t0 = frames(home, pid), time.monotonic()
    time.sleep(seconds)
    f1, t1 = frames(home, pid), time.monotonic()
    return round((f1 - f0) / (t1 - t0), 1) if f0 is not None and f1 is not None else None


def stop(pid):
    os.kill(pid, signal.SIGTERM)
    for _ in range(40):
        try:
            os.kill(pid, 0)
        except OSError:
            return
        time.sleep(0.25)
    os.kill(pid, signal.SIGKILL)


pids = []
try:
    home_a, a = launch('a', 'print', hide_after=20)
    pids.append(a)
    time.sleep(8)
    print('A visible:', {'fps': fps(home_a, a), **surfaces(a)}, flush=True)
    time.sleep(12)
    print('A hidden, at once:', {'fps': fps(home_a, a), **surfaces(a)}, flush=True)
    time.sleep(60)
    print('A hidden, +60 s:', {'fps': fps(home_a, a), **surfaces(a)}, flush=True)
finally:
    for p in pids:
        stop(p)
    # Never deletes (AGENTS.md RULE 1).
    print('run directory kept:', D)
