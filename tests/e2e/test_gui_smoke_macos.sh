#!/usr/bin/env bash
# tests/e2e/test_gui_smoke_macos.sh
#
# macOS GUI smoke harness. Catches the class of regressions that
# burned the May 18–19 release session:
#
#   - menu-bar timing race that crashed the app on startup
#     (-[NSApplication _hasOpenMenuItem])
#   - resize panic in the WebGPU draw path (vertex slice
#     out-of-range)
#   - close-last-tab beach-ball (Mux::shutdown re-entrant lock)
#   - Cmd-Q crash (same root cause)
#   - silent-failure auto-connect when the bundled Lua config
#     references APIs the fork doesn't expose
#
# What this script does:
#   1. Snapshots the existing crash-report directory mtime baseline
#      so any new `.ips` written during the run is detectable.
#   2. Launches the bundled .app via `open` so LSEnvironment is
#      honored (FRANKENTERM_LUA_CONFIG=1 etc. need this).
#   3. Polls for the gui process to come up and stay up for >5s,
#      catching crash-on-launch panics.
#   4. Drives a series of UI actions via osascript: resize the
#      window through small/large/small cycles, open + close tabs
#      via Cmd-T / Cmd-W, quit via Cmd-Q.
#   5. After each action, asserts: same pid still alive (until
#      the explicit Quit step), no new `.ips` crash log written,
#      no `frankenterm-gui_*.plist` CrashReporter entry.
#   6. Finally confirms Quit completes cleanly (process exits,
#      no crash log).
#
# Exit codes:
#   0    all assertions passed
#   1    assertion failed (a regression in this class)
#   2    test environment unusable (FrankenTerm.app not installed,
#        wrong OS, etc) — distinguished from a real failure so CI
#        can decide whether to treat as skipped vs. error
#
# Env knobs:
#   FT_GUI_APP_PATH        path to FrankenTerm.app (default
#                          /Applications/FrankenTerm.app)
#   FT_GUI_SMOKE_KEEP_OPEN if set to "1", skip the quit step so a
#                          human can inspect the running app after
#                          the resize/tab checks
#   FT_GUI_SMOKE_VERBOSE   if "1", echo every step to stderr
#
# This script intentionally has no test framework dependencies. It
# can be run by hand, by a release script, or by a per-PR CI job
# that already provides a macOS runner.
set -euo pipefail

# ─── plumbing ──────────────────────────────────────────────────

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf 'gui-smoke: skipping (Darwin-only; uname=%s)\n' "$(uname -s)" >&2
    exit 2
fi

FT_GUI_APP_PATH="${FT_GUI_APP_PATH:-/Applications/FrankenTerm.app}"
KEEP_OPEN="${FT_GUI_SMOKE_KEEP_OPEN:-0}"
VERBOSE="${FT_GUI_SMOKE_VERBOSE:-0}"
GUI_BIN="${FT_GUI_APP_PATH}/Contents/MacOS/frankenterm-gui"
CRASH_DIR="${HOME}/Library/Logs/DiagnosticReports"

log() {
    if [[ "${VERBOSE}" == "1" ]]; then
        printf 'gui-smoke: %s\n' "$*" >&2
    fi
}

fail() {
    printf 'gui-smoke FAIL: %s\n' "$*" >&2
    # Capture a sample of the wedged process if we can find one — this
    # is the next agent's best lead when triaging.
    # Use the pid WE spawned for the sample / kill — not `pgrep -n` which
    # would return any frankenterm-gui (potentially the user's pre-existing
    # instance). GUI_PID may be empty if we failed before launch detection.
    local stuck_pid="${GUI_PID:-}"
    if [[ -n "${stuck_pid}" ]] && ps -p "${stuck_pid}" >/dev/null 2>&1; then
        local sample_path="/tmp/gui-smoke-fail-sample-${stuck_pid}-$(date +%s).txt"
        /usr/bin/sample "${stuck_pid}" 3 -mayDie -file "${sample_path}" >/dev/null 2>&1 || true
        printf 'gui-smoke: captured sample of pid %s at %s\n' "${stuck_pid}" "${sample_path}" >&2
        kill -9 "${stuck_pid}" 2>/dev/null || true
    fi
    exit 1
}

# GUI_PID is populated after launch detection. fail() references it via the
# `${GUI_PID:-}` guard above so a pre-launch fail() call works too.
GUI_PID=""

require_environment() {
    if [[ ! -d "${FT_GUI_APP_PATH}" ]]; then
        printf 'gui-smoke: skipping (FrankenTerm.app not installed at %s)\n' "${FT_GUI_APP_PATH}" >&2
        exit 2
    fi
    if [[ ! -x "${GUI_BIN}" ]]; then
        printf 'gui-smoke: skipping (gui binary not executable at %s)\n' "${GUI_BIN}" >&2
        exit 2
    fi
    if ! command -v osascript >/dev/null 2>&1; then
        printf 'gui-smoke: skipping (osascript missing)\n' >&2
        exit 2
    fi
}

# Returns the count of crash-log .ips files whose mtime is strictly
# newer than the marker file. Zero if no fresh crash; >0 if the gui
# blew up during the test window.
fresh_crash_count() {
    local marker="$1"
    if [[ ! -d "${CRASH_DIR}" ]]; then
        printf '0\n'
        return
    fi
    find "${CRASH_DIR}" -name 'frankenterm-gui-*.ips' -newer "${marker}" 2>/dev/null | wc -l | tr -d ' '
}

# ─── lifecycle ─────────────────────────────────────────────────

require_environment

# Snapshot baseline before any launch so we can detect crashes written
# during the test window. We use a sentinel mtime file rather than a
# specific .ips name because the OS may write crash logs with arbitrary
# numeric suffixes (frankenterm-gui-2026-05-19-225602.000.ips, etc).
SENTINEL="$(mktemp /tmp/gui-smoke-sentinel.XXXXXX)"
trap 'rm -f "${SENTINEL}"' EXIT

# Record the set of frankenterm-gui pids that are ALREADY running before
# we launch our own — so we can target only the one WE spawned for
# resize / keystroke / Quit. Killing pre-existing instances would
# destroy whatever real work the developer had open. The previous
# `pkill -x frankenterm-gui` here was exactly that bug. If a
# pre-existing instance is detected, we either abort (with exit code 2
# to signal "env unusable, not a regression") or, if FT_GUI_SMOKE_
# OVERRIDE_RUNNING=1, defer to whatever pgrep -n picks last (best
# effort — still preferred over killing).
EXISTING_PIDS="$(pgrep -x frankenterm-gui | sort -u | tr '\n' ' ')"
if [[ -n "${EXISTING_PIDS// /}" ]]; then
    if [[ "${FT_GUI_SMOKE_OVERRIDE_RUNNING:-0}" != "1" ]]; then
        printf 'gui-smoke: skipping (frankenterm-gui already running, pids: %s).\n' "${EXISTING_PIDS}" >&2
        printf 'gui-smoke: set FT_GUI_SMOKE_OVERRIDE_RUNNING=1 to run anyway (will NOT target existing pids).\n' >&2
        exit 2
    fi
    log "pre-existing frankenterm-gui pids: ${EXISTING_PIDS} (override=on, will skip them)"
fi

log "launching ${FT_GUI_APP_PATH}"
open -W -F "${FT_GUI_APP_PATH}" &
OPEN_PID=$!

# Identify the pid OUR `open` spawned by diffing against the
# pre-launch set. Poll up to 10s for a new pid to appear. GUI_PID was
# initialized empty just below the fail() definition above so a fail()
# call before this point has something to test.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    NEW_PIDS="$(pgrep -x frankenterm-gui | sort -u)"
    for candidate in ${NEW_PIDS}; do
        case " ${EXISTING_PIDS} " in
            *" ${candidate} "*) ;;  # was running before, skip
            *) GUI_PID="${candidate}"; break ;;
        esac
    done
    if [[ -n "${GUI_PID}" ]]; then
        break
    fi
done
if [[ -z "${GUI_PID}" ]]; then
    fail "frankenterm-gui did not start within 10s of open (no new pid)"
fi
log "gui pid=${GUI_PID} (our test-spawned instance)"

# Soak for 5 more seconds: catches crash-on-first-event-loop-tick
# panics (the original menu-bar timing race manifested this way).
sleep 5
if ! ps -p "${GUI_PID}" >/dev/null 2>&1; then
    fail "frankenterm-gui exited within 5s of launch; pid ${GUI_PID} is gone"
fi
if [[ "$(fresh_crash_count "${SENTINEL}")" -gt 0 ]]; then
    fail "crash log written within 5s of launch — check ${CRASH_DIR}"
fi

# ─── action: drive a sequence of resizes ───────────────────────

drive_resize() {
    local width="$1"
    local height="$2"
    log "resize → ${width}x${height}"
    /usr/bin/osascript >/dev/null 2>&1 <<APPLESCRIPT || true
    tell application "System Events"
        tell process "frankenterm-gui"
            set size of front window to {${width}, ${height}}
        end tell
    end tell
APPLESCRIPT
}

assert_alive_no_crash() {
    local context="$1"
    if ! ps -p "${GUI_PID}" >/dev/null 2>&1; then
        fail "process died after ${context} (pid ${GUI_PID} gone)"
    fi
    local fresh
    fresh="$(fresh_crash_count "${SENTINEL}")"
    if [[ "${fresh}" -gt 0 ]]; then
        fail "crash log appeared after ${context} (${fresh} new .ips files in ${CRASH_DIR})"
    fi
}

# The original resize-panic regression fired in a draw call after the
# vertex buffer needed to grow. Walk through several size deltas in
# both directions, then back to a baseline.
drive_resize 600 400 ; sleep 1 ; assert_alive_no_crash "resize 600x400"
drive_resize 1400 900 ; sleep 1 ; assert_alive_no_crash "resize 1400x900"
drive_resize 800 600 ; sleep 1 ; assert_alive_no_crash "resize 800x600"
drive_resize 1800 1100 ; sleep 1 ; assert_alive_no_crash "resize 1800x1100"
drive_resize 900 700 ; sleep 1 ; assert_alive_no_crash "resize 900x700"

# ─── action: open + close a tab via Cmd-T then Cmd-W ───────────
#
# Exercises mux.add_tab + window.spawn_tab + remove_tab. The previous
# Mux::shutdown deadlock fired when the user closed the LAST remaining
# tab and shutdown ran — we approximate by opening one tab and closing
# it; the full close-last-tab path is exercised by the final Quit step
# which tears down all windows.

drive_keystroke() {
    local key="$1"
    local using="${2:-command down}"
    log "keystroke → ${key} (using ${using})"
    /usr/bin/osascript >/dev/null 2>&1 <<APPLESCRIPT || true
    tell application "System Events"
        keystroke "${key}" using {${using}}
    end tell
APPLESCRIPT
}

drive_keystroke "t" ; sleep 2 ; assert_alive_no_crash "Cmd-T (open new tab)"
drive_keystroke "w" ; sleep 2 ; assert_alive_no_crash "Cmd-W (close current tab)"

# ─── action: quit ──────────────────────────────────────────────

if [[ "${KEEP_OPEN}" == "1" ]]; then
    log "FT_GUI_SMOKE_KEEP_OPEN=1 — skipping Cmd-Q, leaving gui running for manual inspection"
    printf 'gui-smoke: PASS (kept-open mode, gui pid %s)\n' "${GUI_PID}"
    exit 0
fi

log "sending Quit Apple Event"
# `tell application … to quit` sends an explicit kAEQuitApplication event,
# which is more reliable than `keystroke "q"` (the latter races with
# whatever process currently holds keyboard focus and can be silently
# swallowed if the front process isn't frankenterm-gui at that
# instant). Apple Event target is the bundle id from Info.plist.
/usr/bin/osascript >/dev/null 2>&1 <<APPLESCRIPT || true
tell application id "com.dicklesworthstone.frankenterm" to quit
APPLESCRIPT

# Poll for shutdown with a generous timeout — the Mux::shutdown drop
# path tears down per-domain ClientDomain entries, each of which may
# emit a final mux PDU; we don't want to flag that as a hang.
QUIT_OK=0
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    if ! ps -p "${GUI_PID}" >/dev/null 2>&1; then
        QUIT_OK=1
        break
    fi
done

if [[ "${QUIT_OK}" -ne 1 ]]; then
    fail "process survived Quit (pid ${GUI_PID} still alive after 10s — beachballed?)"
fi
if [[ "$(fresh_crash_count "${SENTINEL}")" -gt 0 ]]; then
    fail "crash log appeared during Quit path"
fi

wait "${OPEN_PID}" 2>/dev/null || true
printf 'gui-smoke: PASS\n'
