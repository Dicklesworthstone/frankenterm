#!/usr/bin/env python3
"""Scripted vttest driver for the headless conformance runner (ft-yccm0.1.9).

vttest is interactive: it draws a screen and waits for a key. This driver runs
it under a pseudo-terminal, sends the keys a person would press, and records
the bytes vttest writes after each key. The bytes for one key are one
"screen". The Rust runner (frankenterm/term/tests/conformance.rs) feeds the
screens to a fresh terminal one at a time and compares the terminal against
the golden dump after each one.

vttest sends a few queries. The driver answers each with the exact bytes
frankenterm-term sends for it (see REPLIES), so vttest takes the same paths it
takes inside FrankenTerm. Any other query aborts the capture, so a recording
can never bake in a timeout path.

Recordings are captured once and committed. Recapturing replaces evidence: do
it only for a new vttest version or a new session, and give the reason in the
commit body. Two captures can differ by a doubled carriage return where vttest
switches tty output modes mid-stream; that never changes the screen.

Usage:
    capture_vttest.py [--vttest PATH] [--out DIR] [SESSION ...]
    capture_vttest.py --explore KEY ...   # print each screen's text, record nothing
"""

import argparse
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import fcntl
import time

ROWS, COLS = 24, 80
# 80 columns minimum and maximum: frankenterm-term does not resize for DECCOLM,
# and vttest's man page recommends 24x80.80 for such terminals.
GEOMETRY = "24x80.80"
# vttest waits for a key after every screen; this much silence ends a screen.
QUIET_SECONDS = 0.6
# Every vttest menu ends with this prompt.
MENU_PROMPT = re.compile(rb"Enter choice number \(0 - [0-9]+\): $")

# Query -> the reply frankenterm-term writes: DA1 and DA2 from
# terminalstate/mod.rs (Device::Request*DeviceAttributes), DECRQSS for DECSCL
# from terminalstate/performer.rs.
REPLIES = [
    (re.compile(rb"\x1b\[0?c"), b"\x1b[?65;4;18;22;52c"),
    (re.compile(rb"\x1b\[>0?c"), b"\x1b[>1;277;0c"),
    (re.compile(rb'\x1bP\$q"p\x1b\\'), b'\x1bP1$r65;1"p\x1b\\'),
]
# Anything that looks like a query the table cannot answer.
UNANSWERED = [
    re.compile(rb"\x1b\[\??[0-9;]*n"),  # DSR / CPR
    re.compile(rb"\x1b\[=0?c"),  # DA3
    re.compile(rb"\x1b\[[0-9;]*x"),  # DECREQTPARM
    re.compile(rb"\x1bP\$q"),  # DECRQSS
    re.compile(rb"\x1b\[\??[0-9;]*\$p"),  # DECRQM
    re.compile(rb"\x1bZ"),  # DECID / VT52 identify
]

# Each session runs in a fresh vttest. "*" sends RETURN until vttest shows a
# menu prompt again; anything else is sent as typed. Left out, and why:
# - 5 (keyboard) and 11.1.7 (DECUDK) need a person at the keyboard.
# - 6, 11.1.1, 11.8.2 and the 11.8.7 alternate-screen tests send reports or
#   cursor-position queries, which the driver cannot answer without an
#   emulator. The Rust suite asserts replies and the alternate screen directly.
# - 7 (VT52): frankenterm-term has no VT52 mode.
# - 10 (RIS, DECTST): vttest waits on a timer after the reset, so screen
#   boundaries would depend on timing.
SESSIONS = {
    "cursor-movements": ["1\r", "*"],
    "screen-features": ["2\r", "*"],
    "character-sets": ["3\r", "8\r", "*", "9\r", "*", "10\r", "*", "11\r", "*"],
    "double-sized-characters": ["4\r", "*"],
    "vt102-insert-delete": ["8\r", "*"],
    "known-bugs": ["9\r"] + [item for n in range(1, 10) for item in ("%d\r" % n, "*")],
    "vt220-screen-display": ["11\r", "1\r", "2\r", "2\r", "*", "3\r", "*", "4\r", "*",
                             "0\r", "6\r", "*"],
    "iso6429-cursor-movement": ["11\r", "5\r"]
    + [item for n in range(1, 10) for item in ("%d\r" % n, "*")],
    "iso6429-colors": ["11\r", "6\r"]
    + [item for n in (2, 3, 4, 5, 8, 9) for item in ("%d\r" % n, "*")]
    + ["6\r", "1\r", "*", "2\r", "*", "3\r", "*", "0\r"]
    + ["7\r"] + [item for n in range(2, 7) for item in ("%d\r" % n, "*")],
    "iso6429-misc": ["11\r", "7\r"]
    + [item for n in range(2, 7) for item in ("%d\r" % n, "*")]
    + ["1\r", "2\r", "*"],
}


class Vttest:
    def __init__(self, vttest):
        env = dict(os.environ, TERM="vt100", LC_ALL="C", LANG="C")
        env.pop("LC_CTYPE", None)
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(vttest, [vttest, GEOMETRY], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pid, self.fd = pid, fd

    def read_screen(self):
        out = b""
        scanned = 0
        last = time.monotonic()
        while time.monotonic() - last < QUIET_SECONDS:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
            last = time.monotonic()
            scanned = self.answer(out, scanned)
        return out

    def answer(self, out, scanned):
        """Answers every complete query in out[scanned:]; returns the new
        scan offset. A query split across reads is found on the next read."""
        while True:
            found = []
            for pattern, reply in REPLIES:
                match = pattern.search(out, scanned)
                if match:
                    found.append((match.start(), match, reply))
            for pattern in UNANSWERED:
                match = pattern.search(out, scanned)
                if match:
                    found.append((match.start(), match, None))
            if not found:
                # Keep a tail so a query split across reads is still seen.
                return max(scanned, len(out) - 16)
            _, match, reply = min(found, key=lambda entry: entry[0])
            if reply is None:
                raise SystemExit("unanswerable query %r; leave this test out" % match.group(0))
            os.write(self.fd, reply)
            scanned = match.end()

    def send(self, key):
        os.write(self.fd, key.encode("latin-1"))

    def close(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)
        os.close(self.fd)


def escape(data):
    """Bytes -> the .vtrec text form, one token per byte, wrapped for diffs.

    Tokens: \\e (ESC), \\r, \\n, \\t, \\\\ and \\xHH; printable ASCII stands for
    itself. File newlines carry no data. A line never starts with '#' or '@'
    (comments and directives) and never ends with a space (editors strip it).
    """
    lines, line = [], ""

    def finish():
        nonlocal line
        if line.endswith(" "):
            line = line[:-1] + "\\x20"
        lines.append(line)
        line = ""

    for byte in data:
        if byte == 0x1B:
            token = "\\e"
        elif byte == 0x5C:
            token = "\\\\"
        elif byte == 0x0D:
            token = "\\r"
        elif byte == 0x0A:
            token = "\\n"
        elif byte == 0x09:
            token = "\\t"
        elif 0x20 <= byte < 0x7F and not (line == "" and byte in (0x23, 0x40)):
            token = chr(byte)
        else:
            token = "\\x%02x" % byte
        if token == "\\e" and len(line) >= 96:
            finish()
        line += token
        if token == "\\n":
            finish()
    if line:
        finish()
    return lines


def unescape(lines):
    """The inverse of escape(); the Rust runner implements the same rules."""
    out = bytearray()
    simple = {"e": 0x1B, "r": 0x0D, "n": 0x0A, "t": 0x09, "\\": 0x5C}
    for line in lines:
        i = 0
        while i < len(line):
            if line[i] != "\\":
                out.append(ord(line[i]))
                i += 1
            elif line[i + 1] == "x":
                out.append(int(line[i + 2 : i + 4], 16))
                i += 4
            else:
                out.append(simple[line[i + 1]])
                i += 2
    return bytes(out)


def capture(vttest_path, keys, on_screen=None):
    vt = Vttest(vttest_path)
    screens = []

    def record(key):
        screens.append((key, vt.read_screen()))
        if on_screen:
            on_screen(len(screens) - 1, *screens[-1])

    try:
        record("")
        for key in keys:
            if key != "*":
                vt.send(key)
                record(key)
                continue
            while True:
                vt.send("\r")
                record("\r")
                if MENU_PROMPT.search(screens[-1][1]):
                    break
                if len(screens) > 200:
                    raise SystemExit("no menu prompt after 200 screens")
        return screens
    finally:
        vt.close()


def vttest_version(vttest_path):
    out = subprocess.run([vttest_path, "-V"], capture_output=True, text=True)
    return (out.stdout + out.stderr).strip()


def write_recording(path, name, keys, version, screens):
    with open(path, "w", encoding="ascii", newline="\n") as f:
        f.write("# vttest recording for the conformance runner (ft-yccm0.1.9).\n")
        f.write("# Captured by capture_vttest.py; do not edit by hand.\n")
        f.write("@vttest %s\n" % version)
        f.write("@geometry %s %dx%d TERM=vt100 LC_ALL=C\n" % (GEOMETRY, ROWS, COLS))
        f.write("@session %s keys %s\n" % (name, " ".join(repr(k) for k in keys)))
        for index, (key, data) in enumerate(screens):
            f.write("@screen %02d key %s\n" % (index, repr(key)))
            lines = escape(data)
            assert unescape(lines) == data, "escape() must round-trip"
            for line in lines:
                f.write(line + "\n")


def plain(data):
    text = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()#][0-9A-Za-z]|\x1b.", b"", data)
    return text.decode("latin-1")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--vttest", default="vttest")
    parser.add_argument("--out", default=os.path.dirname(os.path.abspath(__file__)))
    parser.add_argument("--explore", action="store_true")
    parser.add_argument("names", nargs="*")
    args = parser.parse_args()

    if args.explore:
        def show(index, key, data):
            print("=== screen %02d key %r (%d bytes)" % (index, key, len(data)))
            print(plain(data), flush=True)

        keys = [k.encode("latin-1").decode("unicode_escape") for k in args.names]
        capture(args.vttest, keys, show)
        return

    version = vttest_version(args.vttest)
    for name in args.names or list(SESSIONS):
        keys = SESSIONS[name]
        screens = capture(args.vttest, keys)
        path = os.path.join(args.out, name + ".vtrec")
        write_recording(path, name, keys, version, screens)
        print("%s: %d screens -> %s" % (name, len(screens), path))


if __name__ == "__main__":
    sys.exit(main())
