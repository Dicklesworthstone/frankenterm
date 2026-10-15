#!/bin/bash
# Times a pane that redraws like an Ink TUI taller than the terminal:
# 50 lines, then a clear, N times. ARM=3j clears with ESC[2J ESC[3J ESC[H,
# ARM=2j with ESC[2J ESC[H only. The pane can only print as fast as the mux
# reads it, so the loop's wall time is the server's cost.
# usage: scripts/measure-mux-esc3j-cost.sh <dir with frankenterm-mux-server> <arm: 3j|2j> [N]
set -u
BIN=$1
ARM=$2
N=${3:-2000}
D=$(mktemp -d /tmp/ftesc-XXXXXX)
mkdir -p "$D/home" "$D/config" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/runtime"
cat > "$D/gen.py" <<EOF
import sys, time
clear = "\x1b[2J\x1b[3J\x1b[H" if "$ARM" == "3j" else "\x1b[2J\x1b[H"
time.sleep(3)
t0 = time.monotonic()
for i in range($N):
    sys.stdout.write("".join("line %06d %03d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n" % (i, j) for j in range(50)))
    sys.stdout.write(clear)
    sys.stdout.flush()
t1 = time.monotonic()
open("$D/result", "w").write("%.3f\n" % (t1 - t0))
time.sleep(600)
EOF
env -i PATH=/usr/bin:/bin HOME="$D/home" XDG_CONFIG_HOME="$D/config" XDG_RUNTIME_DIR="$D/runtime" \
  TMPDIR="$D/tmp" "$BIN/frankenterm-mux-server" --daemonize=false --cwd "$D" -- \
  /usr/bin/python3 "$D/gen.py" > "$D/mux.log" 2>&1 &
PID=$!
for _ in $(seq 1 600); do
  [ -s "$D/result" ] && break
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.5
done
kill "$PID" 2>/dev/null
wait "$PID" 2>/dev/null
echo "arm=$ARM n=$N seconds=$(cat "$D/result" 2>/dev/null || echo none) dir=$D"
