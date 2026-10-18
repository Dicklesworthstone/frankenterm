#!/usr/bin/env bash
# Throughput of a guardian-backed pane against a plain one (ft-2pgul).
#
#   test_guardian_pane_throughput.sh <family dir> [bytes] [host script]
#
# The family dir holds frankenterm-mux-server, frankenterm-pty-guardian and ft
# from one build. Each mode runs in a NEW isolated sandbox (own HOME, XDG
# dirs, sockets, FTG_INSTANCE) like test_mux_guardian_lossless_upgrade.sh, and
# its one pane's child prints <bytes> (default 2,000,000) of 100-column text,
# timing its own writes: PTY backpressure paces those to what the pane's
# reader takes.
#   plain:    the mux server owns the PTY and reads it itself;
#   guardian: a broker + guardian pair owns it (mux-guardian-host.sh install
#             --manager direct), so every record is journaled before delivery.
# Prints one RESULT line per mode. MIN_GUARDIAN_KBPS=N fails the run when the
# guardian pane is slower than N KB/s. Nothing is deleted; every started
# process is stopped.
set -uo pipefail
umask "$(printf '%04o' $(( 8#$(umask) | 8#022 )))"
FAMILY=${1:?family dir}
BYTES=${2:-2000000}
TOOL=${3:-$(cd "$(dirname "$0")/../.." && pwd)/scripts/mux-guardian-host.sh}
[ -x "$TOOL" ] || { echo "FAIL: tool $TOOL not executable"; exit 1; }
case $(uname -s) in Darwin) ROOT=/private/tmp ;; *) ROOT=/tmp ;; esac

measure() {  # <plain|guardian> -> "RESULT: <mode> <bytes> bytes in <s> s = <KB/s> KB/s"
  local mode=$1 S line=""
  S=$ROOT/ftgt-$$-$mode
  mkdir -m 700 "$S" || { echo "FAIL: sandbox $S exists"; return 1; }
  (
    export HOME=$S/h XDG_CONFIG_HOME=$S/h/.config XDG_DATA_HOME=$S/h/.local/share XDG_RUNTIME_DIR=$S/r
    export TMPDIR=$S/h/tmp FTG_INSTANCE=gt FTG_RECOVERY_INTERVAL=600
    mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_CONFIG_HOME/frankenterm"
    chmod 700 "$HOME" "$XDG_RUNTIME_DIR" "$TMPDIR"
    unset WEZTERM_UNIX_SOCKET WEZTERM_PANE FRANKENTERM_UNIX_SOCKET FT_WORKSPACE FT_GRID_ENGINE
    cd "$HOME" || exit 1
    STATE=$HOME/.local/state/frankenterm-guardian-gt
    RUN=$XDG_RUNTIME_DIR/frankenterm-guardian-gt
    [ "$(uname -s)" = Darwin ] && RUN=$STATE/run
    mkdir -p "$RUN" && chmod 700 "$RUN"
    # perl for sub-second timestamps: macOS date has none.
    cat > "$HOME/child.sh" <<'CHILD'
sleep 3
s=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
head -c "$1" /dev/zero | tr '\0' x | fold -w 100
e=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
echo "GT_DONE $s $e"
sleep 600
CHILD
    export FTG_MUX_TOML=$XDG_CONFIG_HOME/frankenterm/mux-server-gt.toml
    cat > "$FTG_MUX_TOML" <<TOML
default_prog = ["/bin/bash", "$HOME/child.sh", "$BYTES"]

[[unix_domains]]
name = "unix"
socket_path = "$RUN/mux.sock"
TOML
    if [ "$mode" = guardian ]; then
      trap '"$TOOL" stop-all --i-accept-pane-loss >/dev/null 2>&1 || true' EXIT
      "$TOOL" install --family "$FAMILY" --manager direct > "$S/install.log" 2>&1 \
        || { echo "FAIL: guardian install (see $S/install.log)"; exit 1; }
    else
      "$FAMILY/frankenterm-mux-server" --daemonize=false --config-file "$FTG_MUX_TOML" > "$S/mux.log" 2>&1 &
      mux=$!
      trap 'kill "$mux" 2>/dev/null; true' EXIT
    fi
    for _ in $(seq 1 900); do
      line=$(WEZTERM_UNIX_SOCKET=$RUN/mux.sock "$FAMILY/ft" robot get-text 0 --tail 20 2>/dev/null \
        | grep -o 'GT_DONE [0-9.]* [0-9.]*' | head -1)
      [ -n "$line" ] && break
      sleep 1
    done
    [ -n "$line" ] || { echo "FAIL: $mode pane never finished (sandbox $S)"; exit 1; }
    echo "$line" | awk -v bytes="$BYTES" -v mode="$mode" \
      '{d=$3-$2; if (d <= 0) d = 0.001; printf "RESULT: %s %d bytes in %.2f s = %.1f KB/s\n", mode, bytes, d, bytes/d/1000}'
  )
}

FAILS=0
plain=$(measure plain) || FAILS=$((FAILS + 1))
echo "$plain"
guardian=$(measure guardian) || FAILS=$((FAILS + 1))
echo "$guardian"
kbps=$(echo "$guardian" | sed -n 's/^RESULT: guardian .* = \([0-9.]*\) KB\/s$/\1/p')
if [ -n "${MIN_GUARDIAN_KBPS:-}" ] && [ -n "$kbps" ] \
  && awk -v k="$kbps" -v m="$MIN_GUARDIAN_KBPS" 'BEGIN { exit !(k < m) }'; then
  echo "FAIL: guardian pane $kbps KB/s < $MIN_GUARDIAN_KBPS KB/s"
  FAILS=$((FAILS + 1))
fi
if [ "$FAILS" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL ($FAILS)"; exit 1; fi
