#!/usr/bin/python3 -I
"""ft-2760r native check: how often a GUI attached to a mux server repaints a
flooding remote pane, against the same pane local to the GUI.

remote: a private frankenterm-mux-server (own HOME, XDG dirs and socket)
whose only pane prints numbered lines at full speed; a GUI attaches with
`connect` over the unix domain, so every new frame needs a server push.
local: the same GUI runs the same producer in its own local pane.

The window sits at the bottom edge of the main screen so part of it is
always visible (FT_PR_Y pixels from the top; on mac-mini-old a full-width
Ghostty window covers the rest), the app stays inactive. After WARM seconds
the GUI's presented frames are counted for SECS seconds. Everything runs
under throwaway HOMEs, processes are stopped by exact pid, the run directory
is kept.

usage: push_rate.py APP [label] [secs] [warm]
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
SECS = float(sys.argv[3]) if len(sys.argv) > 3 else 15
WARM = float(sys.argv[4]) if len(sys.argv) > 4 else 5
MUX = os.environ.get('FT_PR_MUX') or os.path.join(APP, 'Contents/MacOS/frankenterm-mux-server')
PRODUCER = ['/usr/bin/perl', '-e',
            '$|=1; my $i=0; while(1){ print $i++, " ", "x" x 100, "\\n" }']
D = tempfile.mkdtemp(prefix='ftpr.', dir='/tmp')
SOCK = os.path.join(D, 's.sock')
Y = os.environ.get('FT_PR_Y', '1120')

LUA = r"""
local ft = require 'frankenterm'
local c = ft.config_builder()
if ft.home_dir ~= os.getenv('FT_PR_HOME') then os.exit(3) end
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
c.front_end = 'WebGpu'
c.max_fps = 60
c.font_size = 10
c.initial_cols = 120
c.initial_rows = 20
c.unix_domains = { { name = 'pr', socket_path = os.getenv('FT_PR_SOCK'), no_serve_automatically = true } }
local function place()
  ft.time.call_after(1, function()
    for _, w in ipairs(ft.gui.gui_windows()) do
      w:set_position(80, tonumber(os.getenv('FT_PR_Y')))
    end
  end)
end
ft.on('gui-attached', place)
ft.on('gui-startup', function(cmd)
  if os.getenv('FT_PR_MODE') == 'local' then
    ft.mux.spawn_window { args = { '/usr/bin/perl', '-e',
      '$|=1; my $i=0; while(1){ print $i++, " ", "x" x 100, "\n" }' } }
    place()
  end
end)
return c
"""
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


def stop(pid):
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        return
    for _ in range(40):
        try:
            os.kill(pid, 0)
        except OSError:
            return
        time.sleep(0.25)
    os.kill(pid, signal.SIGKILL)


def server():
    home = os.path.join(D, 'srv')
    for sub in ('home', 'config', 'cache', 'data', 'state', 'runtime', 'tmp'):
        os.makedirs(os.path.join(home, sub), exist_ok=True)
    os.chmod(os.path.join(home, 'runtime'), 0o700)
    conf = os.path.join(home, 'frankenterm.toml')
    with open(conf, 'w') as f:
        f.write(f'[[unix_domains]]\nname = "pr"\nsocket_path = "{SOCK}"\nno_serve_automatically = true\n')
    env = {
        'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'LANG': 'C', 'HOME': f'{home}/home',
        'XDG_CONFIG_HOME': f'{home}/config', 'XDG_CACHE_HOME': f'{home}/cache',
        'XDG_DATA_HOME': f'{home}/data', 'XDG_STATE_HOME': f'{home}/state',
        'XDG_RUNTIME_DIR': f'{home}/runtime', 'TMPDIR': f'{home}/tmp',
        'WEZTERM_UNIX_SOCKET': SOCK, 'FRANKENTERM_UNIX_SOCKET': SOCK,
        'FRANKENTERM_CONFIG_FILE': conf, 'RUST_LOG': 'warn',
    }
    # Server-side experiment and diagnostic switches pass through.
    env.update({k: v for k, v in os.environ.items() if k.startswith(('FT_EXPERIMENT_', 'FT_DIAG_'))})
    log = open(os.path.join(home, 'mux.log'), 'w')
    proc = subprocess.Popen([MUX, '--config-file', conf, '--daemonize=false', '--cwd', home, '--']
                            + PRODUCER, env=env, stdout=log, stderr=log)
    for _ in range(150):
        if os.path.exists(SOCK):
            return proc
        time.sleep(0.2)
    stop(proc.pid)
    raise SystemExit('mux server never listened')


def gui(mode):
    home = os.path.join(D, f'gui-{mode}')
    os.makedirs(home)
    args = ['connect', 'pr'] if mode == 'remote' else ['start']
    subprocess.run(['open', '-g', '-n', '--env', f'HOME={home}', '--env', f'FT_PR_HOME={home}',
                    '--env', 'FRANKENTERM_LUA_CONFIG=1', '--env', f'FT_PR_SOCK={SOCK}',
                    '--env', f'FT_PR_MODE={mode}', '--env', f'FT_PR_Y={Y}',
                    '--env', 'FT_RESOURCE_SNAPSHOT_INTERVAL_MS=500',
                    '--stderr', f'{home}/err', '-a', APP, '--args', '--config-file', CONF] + args,
                   check=True)
    for _ in range(80):
        pid = gui_pid(home)
        if pid and frames(home, pid) is not None:
            return home, pid
        time.sleep(0.25)
    raise SystemExit(f'{mode} GUI did not start')


def measure(mode):
    home, pid = gui(mode)
    try:
        time.sleep(WARM)
        f0, t0 = frames(home, pid), time.monotonic()
        time.sleep(SECS)
        f1, t1 = frames(home, pid), time.monotonic()
        return round((f1 - f0) / (t1 - t0), 1)
    finally:
        stop(pid)


srv = server()
try:
    remote = measure('remote')
finally:
    srv.terminate()
    try:
        srv.wait(10)
    except subprocess.TimeoutExpired:
        srv.kill()
local = measure('local')
print(f'{LABEL}: remote flooding pane {remote} frames/s, local {local} frames/s', flush=True)
# Never deletes (AGENTS.md RULE 1).
print('run directory kept:', D)
