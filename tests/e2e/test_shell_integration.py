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


if __name__ == "__main__":
    unittest.main()
