"""Exercise the installed prompt snippets with real shells and filesystem paths."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from urllib.parse import quote_from_bytes

SNIPPETS = Path(__file__).resolve().parents[2] / "crates/frankenterm-core/src/shell_integration"


class WorkingDirectoryTests(unittest.TestCase):
    def check_directory(self, shell):
        binary = shutil.which(shell)
        if binary is None:
            self.skipTest(f"{shell} unavailable")
        if shell == "fish":
            script = 'source "$argv[1]"; __ft_prompt_start'
            argv = [binary, "--no-config", "-c", script, str(SNIPPETS / "prompt.fish")]
        else:
            script = 'source "$1"; __ft_current_dir'
            argv = [binary, "-fc", script, shell, str(SNIPPETS / "osc7.sh")]
        with tempfile.TemporaryDirectory() as tmp:
            for dirname in ["plain", "a b#c%d?e", "accént ~ []", "with\nnewline", "single'quote"]:
                with self.subTest(shell=shell, dirname=dirname):
                    path = Path(tmp, dirname).resolve()
                    path.mkdir()
                    result = subprocess.run(argv, cwd=path, capture_output=True, check=True)
                    # The authority comes from the shell/host. Compare the exact URI path.
                    _, start, body = result.stdout.partition(b"\x1b]7;file://")
                    self.assertTrue(start)
                    uri, end, _ = body.partition(b"\x1b\\")
                    self.assertTrue(end)
                    host, slash, encoded = uri.partition(b"/")
                    self.assertRegex(host, rb"^[A-Za-z0-9._-]+$")
                    expected = quote_from_bytes(os.fsencode(path)).encode()
                    self.assertEqual(slash + encoded, expected)

    def test_bash(self):
        self.check_directory("bash")

    def test_zsh(self):
        self.check_directory("zsh")

    def test_fish(self):
        self.check_directory("fish")


@unittest.skipUnless(os.name == 'posix' and shutil.which('bash'), 'requires Bash and a POSIX PTY')
class BashPromptTests(unittest.TestCase):
    def check_prompt(self, prepare, expected_trap=None):
        import pty
        import re
        import select
        import time

        marker = re.compile(rb"\x1b\]133;([ABCD](?:;\d+)?)\x1b\\")
        prompt_end = b"\x1b]133;B\x1b\\"
        with tempfile.TemporaryDirectory() as tmp:
            rc = Path(tmp, "bashrc")
            rc.write_text(
                "PS1='FT> '\n"
                "theme_hook() { printf '<status:%d>' \"$?\"; PS1='THEME> '; }\n"
                "second_hook() { printf '<second>'; }\n"
                "if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then\n"
                "  PROMPT_COMMAND=(theme_hook second_hook)\n"
                "else\n"
                "  PROMPT_COMMAND='theme_hook; second_hook'\n"
                "fi\n"
                + prepare + "\n"
                + 'source "$FT_SNIPPETS/osc7.sh"\n'
                + 'source "$FT_SNIPPETS/prompt.bash"\n' * 2
            )
            master, slave = pty.openpty()
            try:
                with subprocess.Popen(
                    ["bash", "--noprofile", "--rcfile", str(rc), "-i"],
                    stdin=slave, stdout=slave, stderr=slave,
                    env={**os.environ, "FT_SNIPPETS": str(SNIPPETS), "TERM": "dumb",
                         "HISTFILE": os.devnull, "INPUTRC": os.devnull},
                ) as child:
                    def read_prompt():
                        output = bytearray()
                        end = time.monotonic() + 5
                        while prompt_end not in output:
                            remaining = end - time.monotonic()
                            if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                                self.fail(f"Bash did not produce an input boundary: {bytes(output)!r}")
                            output.extend(os.read(master, 4096))
                        return bytes(output)

                    try:
                        first = read_prompt()
                        self.assertEqual(marker.findall(first), [b"A", b"B"])
                        self.assertIn(b"THEME> ", first)
                        os.write(master, b"printf 'franken%s' term; false\n")
                        command = read_prompt()
                        self.assertEqual(marker.findall(command), [b"C", b"D;1", b"A", b"B"])
                        self.assertIn(b"frankenterm", command)
                        self.assertIn(b"<status:1><second>", command)
                        os.write(master, b"\n")
                        self.assertEqual(marker.findall(read_prompt()), [b"A", b"B"])
                        if expected_trap:
                            os.write(master, b"trap -p DEBUG\n")
                            self.assertIn(expected_trap, read_prompt())
                    finally:
                        child.kill()
            finally:
                os.close(master)
                os.close(slave)

    def test_command_lifecycle_with_existing_debug_hook(self):
        # PS0 is available from Bash 4.4. Older Bash can use bash-preexec
        # alongside an existing DEBUG owner.
        version = subprocess.check_output(["bash", "-c", "echo ${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"])
        if tuple(map(int, version.split(b"."))) < (4, 4):
            self.skipTest("requires Bash with PS0")
        self.check_prompt("trap 'original_debug=1' DEBUG", b"original_debug=1")

    def test_upgrade_previous_scalar_hooks(self):
        self.check_prompt(
            "__ft_preexec() { :; }\n__ft_precmd() { :; }\n"
            "PROMPT_COMMAND='__ft_precmd;theme_hook; second_hook'\ntrap '__ft_preexec' DEBUG"
        )

    def test_command_lifecycle_with_debug_fallback(self):
        self.check_prompt("shopt -u promptvars")


if __name__ == "__main__":
    unittest.main()
