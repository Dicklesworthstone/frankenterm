#!/usr/bin/env bash
# A guardian pane through a full disk (ft-ztyt4). macOS only.
#
#   test_guardian_disk_full.sh <family dir> [host script]
#
# The family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build. The run uses a NEW isolated sandbox (own HOME, XDG dirs,
# sockets, FTG_INSTANCE) like test_guardian_pane_throughput.sh, with the
# guardian's state (broker and guardian output journals, recovery store, logs)
# on its own APFS disk image, and one pane whose child prints "ZT <n>" 50 times
# a second. The run fills the image, holds it full for 15 s, then gives space
# back and checks that the pane stalled while full and then resumed with every
# line once, in order. A full APFS volume refuses even truncation, so space
# comes back by growing the container: the image is 400 MB and the container
# starts shrunk to 200 MB. Nothing is deleted: the image (in E2E_IMAGE_DIR,
# default the sandbox root) is only detached, and every started process is
# stopped.
set -uo pipefail
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
FAMILY=${1:?family dir}
TOOL=${2:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ "$(uname -s)" = Darwin ] || { echo "SKIP: needs macOS disk images"; exit 0; }
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 1; }
ROOT=/private/tmp
S=$ROOT/ftdf-$$
IMG=${E2E_IMAGE_DIR:-$ROOT}/ftdf-$$.sparseimage
VOL=/Volumes/ftdf-$$
mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; exit 1; }
hdiutil create -size 400m -type SPARSE -fs APFS -volname "ftdf-$$" "${IMG%.sparseimage}" > "$S/image.log" 2>&1 \
  || { echo "FAIL: create image (see $S/image.log)"; exit 1; }
STORE=$(hdiutil attach "$IMG" -mountpoint "$VOL" -nobrowse 2>> "$S/image.log" | awk '/Apple_APFS/ { print $1 }')
[ -n "$STORE" ] || { echo "FAIL: attach $IMG"; exit 1; }
detach() { hdiutil detach "$VOL" >/dev/null 2>&1 || hdiutil detach -force "$VOL" >/dev/null 2>&1 || true; }
diskutil apfs resizeContainer "$STORE" 200m >> "$S/image.log" 2>&1 \
  || { detach; echo "FAIL: shrink container (see $S/image.log)"; exit 1; }
export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
export TMPDIR=$S/h/tmp FTG_INSTANCE=df FTG_RECOVERY_INTERVAL=600 FTG_STATE=$VOL/state
RUN=$FTG_STATE/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm" "$RUN"
chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$FTG_STATE" "$RUN"
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
cd "$HOME" || exit 1
cat > "$HOME/child.sh" <<'CHILD'
sleep 2
i=0
while :; do i=$((i + 1)); echo "ZT $i"; sleep 0.02; done
CHILD
export FTG_MUX_TOML=$XDG_CONFIG_HOME/frankenterm/mux-server-df.toml
cat > "$FTG_MUX_TOML" <<TOML
default_prog = ["/bin/bash", "$HOME/child.sh"]

[[unix_domains]]
name = "unix"
socket_path = "$RUN/mux.sock"
TOML
trap '"$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true; detach' EXIT
"$TOOL" install --family "$FAMILY" --manager direct > "$S/install.log" 2>&1 \
  || { echo "FAIL: guardian install (see $S/install.log)"; exit 1; }
text() { WEZTERM_UNIX_SOCKET=$RUN/mux.sock "$FAMILY/ft" robot get-text 0 --tail "$1" 2>/dev/null; }
latest() { text 5 | grep -o 'ZT [0-9]*' | tail -1 | cut -d' ' -f2; }
sleep 10
echo "before fill: line $(latest)"
# APFS refuses a 1 MiB write with megabytes still free: top up with ever smaller blocks.
for bs in 1m 64k 4k 512; do dd if=/dev/zero of="$VOL/filler-$bs" bs=$bs >> "$S/fill.log" 2>&1; done
echo "filled: $(df -k "$VOL" | awk 'NR == 2 { print $4 }') KiB free"
# APFS lets a few small writes through on its reserve: keep the longest stall.
L1=$(latest) L2=$L1 run=0 stall=0
for _ in $(seq 1 15); do
  sleep 1
  now=$(latest)
  if [ "$now" = "$L2" ]; then run=$((run + 1)); else run=0; fi
  [ "$run" -gt "$stall" ] && stall=$run
  L2=$now
done
echo "while full: line $L1 -> $L2, longest stall ${stall}s"
diskutil apfs resizeContainer "$STORE" 0 >> "$S/image.log" 2>&1 \
  || { echo "FAIL: grow container (see $S/image.log)"; exit 1; }
echo "space back: $(df -k "$VOL" | awk 'NR == 2 { print $4 }') KiB free"
sleep 20
L3=$(latest)
echo "after: line $L3"
breaks=$(text 4000 | grep -o 'ZT [0-9]*' | cut -d' ' -f2 \
  | awk 'NR > 1 && $1 != prev + 1 { n++; if (n <= 5) printf "%s->%s ", prev, $1 } { prev = $1 } END { printf "| %d", n + 0 }')
echo "sequence breaks: $breaks"
echo "sandbox $S image $IMG"
if [ -n "$L2" ] && [ -n "$L3" ] && [ "$stall" -ge 5 ] && [ "$L3" -gt $(( L2 + 100 )) ] && [ "${breaks##*| }" = 0 ]; then
  echo "RESULT: PASS (stalled ${stall}s at line $L2 while full, resumed to line $L3, no breaks)"
else
  echo "RESULT: FAIL (L1=$L1 L2=$L2 stall=${stall}s L3=$L3 breaks=${breaks##*| })"
  exit 1
fi
