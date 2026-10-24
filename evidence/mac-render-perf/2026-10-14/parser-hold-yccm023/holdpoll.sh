#!/bin/sh
# Start hold_ab.py in the background and print the GUI's parser hold max
# whenever it changes, with seconds since the GUI's snapshot first appeared.
cd ~/.cache/ftcover || exit 2
FT_CV_Y=1960 FT_HA_COLS=240 FT_HA_ROWS=30 /usr/bin/python3 -I hold_ab.py "$1" "$2" poll 24 > /tmp/holdpoll.out 2>&1 &
sleep 2
P=$(pgrep -f "Contents/MacOS/frankenterm-gui" | head -1)
D=$(/bin/ls -td /tmp/ftha.* | head -1)
/usr/bin/python3 -I - "$D/home/.local/share/frankenterm/frankenterm-resources-$P.json" <<'PY'
import json, sys, time
path = sys.argv[1]; t0 = None; last = None
for _ in range(600):
    try:
        d = json.load(open(path))
    except Exception:
        time.sleep(0.1); continue
    t0 = t0 or time.monotonic()
    h = d['terminal_locks']['holders']
    cur = (h['parser']['hold']['max_ns'], h['parser']['hold']['count'], h.get('resize', {}).get('hold', {}).get('max_ns'))
    if cur != last and (last is None or cur[0] != last[0] or cur[2] != last[2]):
        print(f'{time.monotonic()-t0:5.1f}s parser max {cur[0]/1e6:.1f} ms (holds {cur[1]}), resize max {(cur[2] or 0)/1e6:.1f} ms', flush=True)
    last = cur
    time.sleep(0.1)
    if d.get('frames') is None: break
PY
wait
head -1 /tmp/holdpoll.out
