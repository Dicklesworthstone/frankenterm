#!/usr/bin/python3 -I
"""ft-yccm0.7 defect 1, native A/B on this Mac.

Three overlapping 150x40 FrankenTerm windows with the default look (opaque,
window shadow on, WebGpu) print output, so each presents at max_fps. Two
bundles alternate in blocks, plus the new bundle with MACOS_FORCE_DISABLE_SHADOW
as the floor (the bead's acceptance: default decorations within 2 points of it); each block reads WindowServer's and the GUI's
accumulated GPU time (ioreg IOGPUDeviceUserClient AppUsage) and the GUI's own
frames.presented_total. Every GUI runs under a throwaway HOME with an absolute
--config-file, is launched in the background (open -g -n), and is stopped by
the exact pid found by that HOME.

usage: ws_gpu_ab.py OLD.app NEW.app [cycles]
"""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

OLD, NEW = sys.argv[1], sys.argv[2]
CYCLES = int(sys.argv[3]) if len(sys.argv) > 3 else 3
SETTLE, BLOCK = 8, 15

LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_WS_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
c.window_background_opacity = 1.0
c.initial_cols = 150
c.initial_rows = 40
if os.getenv('FT_WS_NOSHADOW') == '1' then
  c.window_decorations = 'TITLE|RESIZE|MACOS_FORCE_DISABLE_SHADOW'
end
local printing = { '/usr/bin/perl', '-e',
  '$|=1; my $i=0; while(1){ print $i++, " ", "x" x 100, "\n"; select(undef,undef,undef,0.004) }' }
ft.on('gui-startup', function()
  for i = 0, 2 do
    ft.mux.spawn_window { args = printing,
      position = { x = 60 + i * 140, y = 60 + i * 70, origin = 'MainScreen' } }
  end
end)
return c
"""

D = tempfile.mkdtemp(prefix='ftws.', dir='/tmp')
CONF = os.path.join(D, 'c.lua')
with open(CONF, 'w') as f:
    f.write(LUA)


def gpu_times():
    out = subprocess.run(['ioreg', '-r', '-l', '-w', '0', '-c', 'IOGPUDeviceUserClient'],
                         capture_output=True, text=True).stdout
    total = {}
    for block in out.split('+-o ')[1:]:
        m = re.search(r'"IOUserClientCreator" = "pid (\d+), ([^"]*)"', block)
        if not m:
            continue
        ns = sum(int(x) for x in re.findall(r'"accumulatedGPUTime"=(\d+)', block))
        pid = int(m.group(1))
        total[pid] = total.get(pid, 0) + ns
        if m.group(2) == 'WindowServer':
            total['ws'] = total.get('ws', 0) + ns
    return total


def gui_pid(home):
    pids = subprocess.run(['pgrep', '-f', 'Contents/MacOS/frankenterm-gui'],
                          capture_output=True, text=True).stdout.split()
    for p in pids:
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


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def stop(pid):
    os.kill(pid, signal.SIGTERM)
    for _ in range(40):
        if not alive(pid):
            return
        time.sleep(0.25)
    os.kill(pid, signal.SIGKILL)


def block(arm, app, n, noshadow):
    home = os.path.join(D, f'h-{arm}-{n}')
    os.makedirs(home)
    subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_WS_HOME={home}',
                    '--env', 'FRANKENTERM_LUA_CONFIG=1', '--env', f'FT_WS_NOSHADOW={int(noshadow)}',
                    '--env', 'RUST_LOG=warn,frankenterm_gui=info',
                    '--stderr', f'{home}/err', '-a', app, '--args', '--config-file', CONF, 'start'],
                   check=True)
    pid = None
    for _ in range(80):
        pid = gui_pid(home)
        if pid and frames(home, pid) is not None:
            break
        time.sleep(0.25)
    if not pid:
        print(f'{arm} {n}: GUI did not start')
        return None
    try:
        time.sleep(SETTLE)
        g0, f0, t0 = gpu_times(), frames(home, pid), time.monotonic()
        time.sleep(BLOCK)
        g1, f1, t1 = gpu_times(), frames(home, pid), time.monotonic()
    finally:
        stop(pid)
    wall_ns = (t1 - t0) * 1e9
    ws = (g1.get('ws', 0) - g0.get('ws', 0)) / wall_ns * 100
    gui = (g1.get(pid, 0) - g0.get(pid, 0)) / wall_ns * 100
    fps = (f1 - f0) / (t1 - t0) if f0 is not None and f1 is not None else -1
    modes = sorted(set(re.findall(r'alpha_mode=(\w+)', open(f'{home}/err', errors='replace').read())))
    row = {'arm': arm, 'block': n, 'windowserver_gpu_pct': round(ws, 1),
           'gui_gpu_pct': round(gui, 1), 'gui_fps': round(fps, 1), 'alpha_modes': modes}
    print(json.dumps(row), flush=True)
    return row


rows = []
try:
    for n in range(1, CYCLES + 1):
        for arm, app, noshadow in (('old', OLD, False), ('new', NEW, False),
                                   ('new-noshadow', NEW, True)):
            rows.append(block(arm, app, n, noshadow))
            time.sleep(4)
finally:
    shutil.rmtree(D, ignore_errors=True)

for arm in ('old', 'new', 'new-noshadow'):
    ws = sorted(r['windowserver_gpu_pct'] for r in rows if r and r['arm'] == arm)
    if ws:
        print(f'{arm}: WindowServer GPU median {ws[len(ws) // 2]}% (range {ws[0]}-{ws[-1]}, {len(ws)} blocks)')
