#!/bin/bash
# Headless drain time of `seq 1 N` in one pane of a private mux server, with
# the durable scrollback store on (default) or off
# (scrollback_tiered_enabled = false). The pane can only print as fast as
# the server reads it, so seq's wall time is the server's ingest cost.
# usage: store-drain-probe.sh <dir with frankenterm-mux-server> <on|on-nogeom|off> [N] [sample-secs]
# With sample-secs, /usr/bin/sample records the server for that long,
# starting 2 s into the flood, into $D/sample.txt.
set -u
BIN=$1
ARM=$2
N=${3:-20000000}
SAMPLE=${4:-0}
D=$(mktemp -d /tmp/ftsd-XXXXXX)
mkdir -p "$D/home" "$D/config/frankenterm" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/runtime"
if [ "$ARM" = off ]; then
  echo 'scrollback_tiered_enabled = false' > "$D/config/frankenterm/frankenterm.toml"
fi
cat > "$D/gen.py" <<EOF
import subprocess, time
time.sleep(3)
t0 = time.monotonic()
subprocess.run(['/usr/bin/seq', '1', '$N'])
open('$D/result', 'w').write('%.3f\n' % (time.monotonic() - t0))
time.sleep(600)
EOF
EXTRA=
[ "$ARM" = on-nogeom ] && EXTRA=FT_SCROLLBACK_COLD_GEOMETRY=0
env -i $EXTRA PATH=/usr/bin:/bin HOME="$D/home" XDG_CONFIG_HOME="$D/config" XDG_RUNTIME_DIR="$D/runtime" \
  TMPDIR="$D/tmp" "$BIN/frankenterm-mux-server" --daemonize=false --cwd "$D" -- \
  /usr/bin/python3 "$D/gen.py" > "$D/mux.log" 2>&1 &
PID=$!
if [ "$SAMPLE" != 0 ]; then
  sleep 5
  /usr/bin/sample "$PID" "$SAMPLE" -f "$D/sample.txt" > /dev/null 2>&1
fi
for _ in $(seq 1 1200); do
  [ -s "$D/result" ] && break
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.25
done
kill "$PID" 2>/dev/null
wait "$PID" 2>/dev/null
echo "arm=$ARM n=$N seconds=$(cat "$D/result" 2>/dev/null || echo none) load=$(sysctl -n vm.loadavg | awk '{print $2}') dir=$D store_mb=$(/usr/bin/du -sm "$D/home" | cut -f1)"
