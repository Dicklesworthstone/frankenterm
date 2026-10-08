#!/usr/bin/env bash
# GUI end-to-end PTY-drain and FPS harness (ft-yccm0.1.4, plan label M.3):
# `time cat <corpus>` in an isolated dev FrankenTerm GUI and in Ghostty.app,
# ABBA-interleaved, with the tty write-offset drain curve, a terminal-agnostic
# ScreenCaptureKit FPS meter, a main-thread beach-ball probe and a receipt.
#
# Usage: scripts/mac-gui-throughput.sh --gui-bin PATH [options]   (--help for all)
#
#   scripts/mac-gui-throughput.sh --gui-bin BIN --corpus-gen-bin INGEST_EXAMPLE
#       T0 (color_emoji_random, the operator's 749,801,000-byte size), FrankenTerm
#       vs Ghostty, two runs each (ABBA)
#   scripts/mac-gui-throughput.sh --gui-bin BIN --corpus-file ~/color-emoji-random.bin
#       the operator's own file, read-only
#   scripts/mac-gui-throughput.sh --gui-bin BIN --corpus-gen-bin X --baseline ft \
#       --baseline-ft-lua 'config.some_option = false'
#       FrankenTerm A/B (e.g. the A1.8 durable-store gate)
#   scripts/mac-gui-throughput.sh --gui-bin BIN --corpus-gen-bin X --baseline ft --sibling-flood
#       pane 1 floods T1 while pane 2 is measured (L2/A2 gates)
#   scripts/mac-gui-throughput.sh --gui-bin BIN --corpus-file ~/color-emoji-random.bin --cpu-hog 0,12
#       the same ABBA with no hog, then with 12 busy default-QoS processes during
#       both arms: the FrankenTerm/Ghostty ratio at each (ft-yccm0.6)
#   scripts/mac-gui-throughput.sh --gui-bin BIN --self-test    1 MiB, one run per arm
#   scripts/mac-gui-throughput.sh --dry-run ...                the plan only
#   scripts/mac-gui-throughput.sh --analysis-self-test         offline checks only
#
# Needs: macOS, python3, Xcode's swiftc (the frame meter is compiled into
# $TMPDIR/ft-gui-throughput-cache), and Screen Recording plus Accessibility
# permission for the app this runs in; the harness checks both up front and
# refuses to run without them rather than report 0 FPS. FrankenTerm runs with
# max_fps at twice the display refresh rate by default (--ft-max-fps N to
# override): builds before 2aa4db498 (ft-1w85m) throttle to whole
# milliseconds, so max_fps equal to the refresh rate capped them below the
# display; the operator's 30 caps any build below Ghostty by construction.
#
# Output: <run dir>/run.log and <run dir>/receipt.json (schema
# frankenterm.gui_throughput_receipt.v1), one directory per run under runs/.
# Exit 0 receipt written, 1 a run failed or the receipt is invalid, 2 refused.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[[ "$(uname -s)" == "Darwin" ]] || { echo "mac-gui-throughput: macOS only" >&2; exit 2; }
command -v python3 >/dev/null || { echo "mac-gui-throughput: python3 is required" >&2; exit 2; }
exec python3 -I "$SCRIPT_DIR/mac-gui-throughput.py" "$@"
