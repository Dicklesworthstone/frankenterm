#!/usr/bin/python3 -I
"""ft-yccm0.2.1.8 durable-store A/B: GUI drain time with the store on and off.

One GUI per run, one window (placed like hold_ab.py so it presents frames)
whose pane times a single `cat CORPUS` (or `seq 1 N` when CORPUS is
seq:N) and writes the seconds to a file. cat can only write as fast as the
GUI reads the pty, so that time is the GUI's drain time. Arms: "on" is the
default configuration, "off" sets scrollback_tiered_enabled = false (no warm
or cold tier, nothing reaches the durable store). Runs alternate ABBA.

The GUI runs under its own throwaway HOME with an absolute --config-file,
launched with `open -g -n`, and is stopped by the exact pid found by that
HOME. Run directories are kept (AGENTS.md RULE 1).

usage: drain_ab.py APP CORPUS ROUNDS [label]
With DA_ARMS="name=APP:on|off,..." the runs rotate through those arms
instead (each round reverses the previous order), e.g. two builds' store-on
arms beside one build's store-off arm.
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time

APP, CORPUS, ROUNDS = sys.argv[1], sys.argv[2], int(sys.argv[3])
LABEL = sys.argv[4] if len(sys.argv) > 4 else os.path.basename(CORPUS)
if not CORPUS.startswith('seq:'):
    CORPUS = os.path.abspath(CORPUS)
LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_DA_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
c.font_size = 10
c.initial_cols = 240
c.initial_rows = 30
c.window_decorations = 'RESIZE|MACOS_FORCE_DISABLE_SHADOW'
if os.getenv('FT_DA_ARM') == 'off' then
  c.scrollback_tiered_enabled = false
end
local timer = [==[
import subprocess, sys, time
src, out = sys.argv[1], sys.argv[2]
time.sleep(1)
cmd = ['/usr/bin/seq', '1', src[4:]] if src.startswith('seq:') else ['/bin/cat', src]
t = time.monotonic()
subprocess.run(cmd)
open(out, 'w').write('%.4f\n' % (time.monotonic() - t))
time.sleep(600)
]==]
ft.on('gui-startup', function()
  local _, _, window = ft.mux.spawn_window {
    args = { '/usr/bin/python3', '-I', '-c', timer, os.getenv('FT_DA_CORPUS'), os.getenv('FT_DA_OUT') },
  }
  local x = tonumber(os.getenv('FT_CV_X') or '') or 80
  local y = tonumber(os.getenv('FT_CV_Y') or '') or 1960
  ft.time.call_after(0.5, function()
    window:gui_window():set_position(x, y)
    window:gui_window():focus()
  end)
end)
return c
"""


def gui_pid(home):
    for p in subprocess.run(['pgrep', '-f', 'Contents/MacOS/frankenterm-gui'],
                            capture_output=True, text=True).stdout.split():
        cmd = subprocess.run(['ps', '-E', '-ww', '-p', p, '-o', 'command='],
                             capture_output=True, text=True).stdout
        if re.search(r'(^| )HOME=' + re.escape(home) + r'( |$)', cmd):
            return int(p)
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


def du_mb(path):
    out = subprocess.run(['/usr/bin/du', '-sk', path], capture_output=True, text=True).stdout
    return int(out.split()[0]) // 1024 if out else -1


def run(arm, app=APP, name=None):
    name = name or arm
    d = tempfile.mkdtemp(prefix=f'ftda.{name}.', dir='/tmp')
    conf, home, out = os.path.join(d, 'c.lua'), os.path.join(d, 'home'), os.path.join(d, 'secs')
    os.makedirs(home)
    with open(conf, 'w') as f:
        f.write(LUA)
    load = os.getloadavg()[0]
    subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_DA_HOME={home}',
                    '--env', 'FRANKENTERM_LUA_CONFIG=1', '--env', f'FT_DA_CORPUS={CORPUS}',
                    '--env', f'FT_DA_OUT={out}', '--env', f'FT_DA_ARM={arm}',
                    '--env', f'FT_CV_Y={os.environ.get("FT_CV_Y", "")}',
                    '--env', 'RUST_LOG=warn', '--stderr', f'{home}/err',
                    '-a', app, '--args', '--config-file', conf, 'start'], check=True)
    pid = None
    for _ in range(80):
        pid = gui_pid(home)
        if pid:
            break
        time.sleep(0.25)
    else:
        raise SystemExit('the GUI did not start')
    try:
        for _ in range(1200):
            if _ == 20 and 'Configuration Error' in open(f'{home}/err', errors='replace').read():
                raise SystemExit(f'{arm}: the GUI rejected the config ({d}/home/err)')
            if os.path.exists(out) and os.path.getsize(out) > 0:
                break
            time.sleep(0.25)
        else:
            raise SystemExit(f'{arm}: no result after 300 s ({d})')
        secs = float(open(out).read())
        time.sleep(2)  # let the durability writer settle before sizing the store
        snap = None
        try:
            snap = json.load(open(f'{home}/.local/share/frankenterm/frankenterm-resources-{pid}.json'))
        except (OSError, ValueError):
            pass
    finally:
        stop(pid)
    fps = None
    if snap and snap.get('frames'):
        fps = snap['frames'].get('presented_total')
    return {'arm': name, 'secs': secs, 'load_1m': round(load, 2), 'dir': d,
            'dir_mb': du_mb(d), 'presented_total': fps}


results = []
ARMS = [a for a in os.environ.get('DA_ARMS', '').split(',') if a]
if ARMS:
    arms = []
    for spec in ARMS:
        name, rest = spec.split('=', 1)
        app, store = rest.rsplit(':', 1)
        arms.append((name, app, store))
    for r in range(ROUNDS):
        for name, app, store in (arms if r % 2 == 0 else arms[::-1]):
            rec = run(store, app, name)
            rec.update(label=LABEL, round=r)
            results.append(rec)
            print(json.dumps(rec), flush=True)
    for name, _, _ in arms:
        v = sorted(x['secs'] for x in results if x['arm'] == name)
        print(f'{LABEL}: {name} median {v[len(v) // 2] if len(v) % 2 else (v[len(v) // 2 - 1] + v[len(v) // 2]) / 2:.3f} s {v}', flush=True)
    sys.exit(0)
for r in range(ROUNDS):
    order = ('on', 'off') if r % 2 == 0 else ('off', 'on')
    for arm in order:
        rec = run(arm)
        rec.update(label=LABEL, round=r)
        results.append(rec)
        print(json.dumps(rec), flush=True)
on = sorted(x['secs'] for x in results if x['arm'] == 'on')
off = sorted(x['secs'] for x in results if x['arm'] == 'off')
med = lambda v: v[len(v) // 2] if len(v) % 2 else (v[len(v) // 2 - 1] + v[len(v) // 2]) / 2
pairs = [a['secs'] / b['secs'] for a, b in
         ((next(x for x in results if x['round'] == r and x['arm'] == 'on'),
           next(x for x in results if x['round'] == r and x['arm'] == 'off')) for r in range(ROUNDS))]
print(f'{LABEL}: on median {med(on):.3f} s {on}; off median {med(off):.3f} s {off}; '
      f'on/off per-round {[round(p, 3) for p in pairs]} median {med(sorted(pairs)):.3f}', flush=True)
