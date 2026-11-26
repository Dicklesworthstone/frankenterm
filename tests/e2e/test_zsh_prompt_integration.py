#!/usr/bin/env python3
"""Exercise the installed Zsh shell integration on a real interactive PTY."""
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "crates/frankenterm-core/src/shell_integration"
MARKER = re.compile(rb"\x1b\]133;([ABCD](?:;[0-9]+)?)\x1b\\")
END_PROMPT = b"\x1b]133;B\x1b\\"


class ZshPromptTest(unittest.TestCase):
    def test_prompt_and_command_boundaries(self):
        zsh = shutil.which("zsh")
        self.assertIsNotNone(zsh, "running this interactive test requires zsh")
        with tempfile.TemporaryDirectory() as home:
            directory = Path(home) / "work dir"
            directory.mkdir()
            rc = """
unsetopt promptsp
setopt promptsubst
__theme_precmd() {
    print -r -- "theme precmd $?"
    PS1='visible %? > '
}
__theme_preexec() { print -r -- 'theme preexec'; }
precmd_functions=(__theme_precmd)
preexec_functions=(__theme_preexec)
source "$FT_TEST_SCRIPTS/osc7.sh"
source "$FT_TEST_SCRIPTS/prompt.zsh"
source "$FT_TEST_SCRIPTS/prompt.zsh"
"""
            (Path(home) / ".zshrc").write_text(rc, encoding="utf-8")
            pid, master = pty.fork()
            if pid == 0:
                os.chdir(directory)
                os.execve(zsh, [zsh, "-d", "-i"], {
                    "HOME": home, "ZDOTDIR": home, "FT_TEST_SCRIPTS": str(SCRIPTS),
                    "TERM": "xterm", "PATH": os.defpath, "LC_ALL": "C",
                })
            try:
                def read_prompt():
                    buf = bytearray()
                    deadline = time.monotonic() + 5
                    while END_PROMPT not in buf:
                        left = deadline - time.monotonic()
                        if left <= 0 or not select.select([master], [], [], left)[0]:
                            self.fail(f"Zsh prompt timed out: {bytes(buf)!r}")
                        chunk = os.read(master, 8192)
                        if not chunk:
                            self.fail(f"Zsh ended before its prompt: {bytes(buf)!r}")
                        buf.extend(chunk)
                    return bytes(buf)

                startup = read_prompt()
                self.assertEqual(MARKER.findall(startup), [b"A", b"B"])
                self.assertRegex(startup, rb"\x1b\]133;A\x1b\\visible 0 > \x1b\]133;B\x1b\\")
                self.assertIn(b"work%20dir", startup)
                os.write(master, b"print -r -- 'command output'; (exit 42)\n")
                failed = read_prompt()
                self.assertEqual(MARKER.findall(failed), [b"C", b"D;42", b"A", b"B"])
                c = failed.index(b"\x1b]133;C")
                d = failed.index(b"\x1b]133;D;42")
                self.assertIn(b"theme preexec", failed[c:d])
                self.assertIn(b"command output\r\n", failed[c:d])
                self.assertIn(b"theme precmd 42", failed[d:])
                self.assertIn(b"visible 42 > ", failed[d:])

                os.write(master, b"\n")
                blank = read_prompt()
                self.assertEqual(MARKER.findall(blank), [b"A", b"B"])

                # Remove the theme's hook and extend its prompt from a command.
                # The command must see the theme text without our decorations.
                os.write(master, b'precmd_functions[${precmd_functions[(i)__theme_precmd]}]=(); PS1+="custom "; true\n')
                success = read_prompt()
                self.assertEqual(MARKER.findall(success), [b"C", b"D;0", b"A", b"B"])
                self.assertIn(b"visible 0 > custom ", success)
                os.write(master, b"\n")
                self.assertEqual(MARKER.findall(read_prompt()), [b"A", b"B"])
            finally:
                os.close(master)
                # A broken hook must not strand an interactive shell in CI.
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(pid, 0)


if __name__ == "__main__":
    unittest.main()
