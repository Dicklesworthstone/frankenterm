#!/usr/bin/env bash
# measure-mux-write-amplification.sh — how many bytes does frankenterm-mux-server
# write to disk per byte a pane prints? (ft-14kx5, ft-y0gy9)
#
# Starts a private mux (isolated HOME/XDG/TMPDIR, no clients) whose only pane
# runs GEN, waits for the output to settle, and compares the mux process's own
# disk writes (proc_pid_rusage ri_diskio_byteswritten; macOS) with the bytes
# GEN prints. Never touches a running FrankenTerm.
#
# usage: [GEN='<shell command>'] scripts/measure-mux-write-amplification.sh [BIN_DIR]
#   BIN_DIR  directory holding frankenterm-mux-server (default target/release)
#   GEN      pane command (default: seq 1 2000000)
set -u
umask 077

[[ "$(uname -s)" == Darwin ]] || { echo "SKIP: uses macOS proc_pid_rusage" >&2; exit 2; }
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$(cd "${1:-$REPO_ROOT/target/release}" && pwd)"
MUX="$BIN/frankenterm-mux-server"
[[ -x "$MUX" ]] || { echo "SKIP: $MUX not built" >&2; exit 2; }
GEN=${GEN:-seq 1 2000000}

D=$(mktemp -d /tmp/ftwa-XXXXXX)
mkdir -p "$D/home" "$D/config" "$D/runtime" "$D/tmp"
chmod 700 "$D" "$D/runtime"
SOCK="$D/mux.sock"
printf '[[unix_domains]]\nname = "wa"\nsocket_path = "%s"\nno_serve_automatically = true\n' "$SOCK" > "$D/frankenterm.toml"
EMITTED=$(/bin/zsh -f -c "$GEN" | wc -c | tr -d ' ')

env -i PATH=/usr/bin:/bin HOME="$D/home" XDG_CONFIG_HOME="$D/config" XDG_RUNTIME_DIR="$D/runtime" \
  TMPDIR="$D/tmp" FRANKENTERM_CONFIG_FILE="$D/frankenterm.toml" LANG=C \
  "$MUX" --config-file "$D/frankenterm.toml" --daemonize=false --cwd "$D" -- \
  /bin/zsh -f -c "sleep 3; $GEN; sleep 600" > "$D/mux.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2> /dev/null' EXIT

rusage() { # pid -> "<disk bytes written> <phys footprint>"
  python3 - "$1" << 'PY'
import ctypes, sys
class RU(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(n, ctypes.c_uint64) for n in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins",
        "wired_size", "resident_size", "phys_footprint", "proc_start_abstime",
        "proc_exit_abstime", "child_user_time", "child_system_time",
        "child_pkg_idle_wkups", "child_interrupt_wkups", "child_pageins",
        "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten")]
ru = RU()
if ctypes.CDLL("/usr/lib/libproc.dylib").proc_pid_rusage(int(sys.argv[1]), 2, ctypes.byref(ru)):
    print("0 0"); sys.exit()
print(ru.diskio_byteswritten, ru.phys_footprint)
PY
}

sleep 2
read -r W0 _ <<< "$(rusage "$PID")"
S0=$(du -sk "$D" | cut -f1)
# Let the pane start printing, wait until the mux goes idle, then settle.
sleep 5
for _ in $(seq 1 180); do
  CPU=$(ps -o %cpu= -p "$PID" | tr -d ' ' | cut -d. -f1)
  [[ "${CPU:-0}" -lt 5 ]] && break
  sleep 1
done
sleep 10
read -r W1 FOOT <<< "$(rusage "$PID")"
S1=$(du -sk "$D" | cut -f1)

python3 - "$EMITTED" "$W0" "$W1" "$S0" "$S1" "$FOOT" "$GEN" << 'PY'
import json, sys
emitted, w0, w1, s0, s1, foot = map(int, sys.argv[1:7])
written = w1 - w0
print(f"pane command      {sys.argv[7]}")
print(f"pane emitted      {emitted/1e6:8.2f} MB")
print(f"mux disk writes   {written/1e6:8.2f} MB  ({written/emitted:.2f}x of emitted)")
print(f"workspace grew    {(s1-s0)/1e3:8.2f} MB")
print(f"mux footprint     {foot/1e6:8.2f} MB")
print(json.dumps({"emitted_bytes": emitted, "mux_disk_bytes_written": written,
                  "amplification": round(written / emitted, 2)}))
PY
echo "workspace: $D"
