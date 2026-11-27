"""Exercise the installed prompt snippets with real shells and filesystem paths."""
import os
from pathlib import Path
import re
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

    def test_system_bash(self):
        # macOS ships Bash 3.2 as /bin/bash, usually behind a newer Bash on PATH.
        self.check_directory("/bin/bash")

    def test_zsh(self):
        self.check_directory("zsh")

    def test_fish(self):
        self.check_directory("fish")


MARKER = re.compile(rb"\x1b\]133;([ABCD](?:;\d+)?)\x1b\\")
SOURCE_SNIPPETS = 'source "$FT_SNIPPETS/osc7.sh"\nsource "$FT_SNIPPETS/prompt.bash"\n'
# macOS ships Bash 3.2 as /bin/bash, usually behind a newer Bash on PATH.
# Bash 3.2 has no PS0, so it takes the DEBUG-trap fallback.
BASH_BINARIES = ["bash", "/bin/bash"]


def bash_version(bash):
    output = subprocess.check_output([bash, "-c", "echo ${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"])
    return tuple(map(int, output.split(b".")))


@unittest.skipUnless(os.name == 'posix' and shutil.which('bash'), 'requires Bash and a POSIX PTY')
class BashPromptTests(unittest.TestCase):
    def run_session(self, bash, rc_text, lines):
        """Start an interactive Bash on a PTY with rc_text as its rc file, then
        send each line. Return the output up to each input boundary: the first
        prompt, then one per line."""
        import pty
        import select
        import time

        if shutil.which(bash) is None:
            self.skipTest(f"{bash} unavailable")
        prompt_end = b"\x1b]133;B\x1b\\"
        with tempfile.TemporaryDirectory() as tmp:
            rc = Path(tmp, "bashrc")
            rc.write_text(rc_text)
            master, slave = pty.openpty()
            try:
                with subprocess.Popen(
                    [bash, "--noprofile", "--rcfile", str(rc), "-i"],
                    stdin=slave, stdout=slave, stderr=slave,
                    # Under TERM=dumb, Bash 3.2's readline redraws the first
                    # prompt line, repeating its markers. Model a real terminal.
                    env={**os.environ, "FT_SNIPPETS": str(SNIPPETS), "FT_RC": str(rc),
                         "TERM": "xterm-256color", "HISTFILE": os.devnull,
                         "INPUTRC": os.devnull},
                ) as child:
                    def read_prompt():
                        output = bytearray()
                        end = time.monotonic() + 5
                        while prompt_end not in output:
                            remaining = end - time.monotonic()
                            if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                                self.fail(f"{bash} did not produce an input boundary: {bytes(output)!r}")
                            output.extend(os.read(master, 4096))
                        return bytes(output)

                    try:
                        outputs = [read_prompt()]
                        for line in lines:
                            os.write(master, line + b"\n")
                            outputs.append(read_prompt())
                        return outputs
                    finally:
                        child.kill()
            finally:
                os.close(master)
                os.close(slave)

    def check_prompt(self, prepare, expected_trap=None, after="", bash="bash",
                     check_underscore=True):
        rc_text = (
            "PS1='FT> '\n"
            "theme_hook() { printf '<status:%d>' \"$?\"; PS1='THEME> '; }\n"
            "second_hook() { printf '<second>'; }\n"
            "if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then\n"
            "  PROMPT_COMMAND=(theme_hook second_hook)\n"
            "else\n"
            "  PROMPT_COMMAND='theme_hook; second_hook'\n"
            "fi\n"
            + prepare + "\n"
            + SOURCE_SNIPPETS * 2
            + after + "\n"
        )
        lines = [b"printf 'franken%s' term; false", b""]
        if check_underscore:
            # The DEBUG trap runs between these two commands.
            lines.append(b"true underscored; printf '<last:%s>' \"$_\"")
        if expected_trap:
            lines.append(b"trap -p DEBUG")
        first, command, empty, *rest = self.run_session(bash, rc_text, lines)
        self.assertEqual(MARKER.findall(first), [b"A", b"B"], first)
        self.assertIn(b"THEME> ", first)
        self.assertEqual(MARKER.findall(command), [b"C", b"D;1", b"A", b"B"], command)
        self.assertIn(b"frankenterm", command)
        self.assertIn(b"<status:1><second>", command)
        self.assertEqual(MARKER.findall(empty), [b"A", b"B"], empty)
        if check_underscore:
            # The integration must not replace $_ with one of its own words.
            underscore = rest[0]
            self.assertIn(b"<last:underscored>", underscore)
            self.assertEqual(MARKER.findall(underscore), [b"C", b"D;0", b"A", b"B"], underscore)
        if expected_trap:
            self.assertIn(expected_trap, rest[-1])

    def test_command_lifecycle_with_existing_debug_hook(self):
        # PS0 is available from Bash 4.4. Older Bash can use bash-preexec
        # alongside an existing DEBUG owner.
        if bash_version("bash") < (4, 4):
            self.skipTest("requires Bash with PS0")
        # The user's own trap decides $_ here.
        self.check_prompt("trap 'original_debug=1' DEBUG", b"original_debug=1",
                          check_underscore=False)

    def check_upgrade(self, prepare):
        # An earlier version's DEBUG trap replaced $_ with its own words, and
        # a re-source cannot see it. Bash 4+ lets the new hook replace it.
        # Bash 3.2 restores it after every function, so only the markers hold.
        for bash in BASH_BINARIES:
            with self.subTest(bash=bash):
                if shutil.which(bash) is None:
                    self.skipTest(f"{bash} unavailable")
                self.check_prompt(prepare, bash=bash,
                                  check_underscore=bash_version(bash) >= (4, 0))

    def test_upgrade_previous_scalar_hooks(self):
        self.check_upgrade(
            "__ft_preexec() { :; }\n__ft_precmd() { :; }\n"
            "PROMPT_COMMAND='__ft_precmd;theme_hook; second_hook'\n"
            "trap '__ft_preexec' DEBUG"
        )

    def test_upgrade_previous_debug_fallback_trap(self):
        self.check_upgrade("trap '[[ $BASH_COMMAND == __ft_precmd ]] || __ft_preexec' DEBUG")

    def test_command_lifecycle_with_debug_fallback(self):
        for bash in BASH_BINARIES:
            with self.subTest(bash=bash):
                self.check_prompt("shopt -u promptvars", bash=bash)

    def test_command_lifecycle(self):
        for bash in BASH_BINARIES:
            with self.subTest(bash=bash):
                self.check_prompt("", bash=bash)

    def test_prompt_hook_added_after_the_block(self):
        # rc files often append a hook after the ft block. On the DEBUG-trap
        # fallback that hook runs after the prompt is finished and must not
        # pass for the user's command.
        after = (
            "late_hook() { printf '<late>'; }\n"
            "if [[ $(declare -p PROMPT_COMMAND) == 'declare -a'* ]]; then\n"
            "  PROMPT_COMMAND+=(late_hook)\n"
            "else\n"
            "  PROMPT_COMMAND=\"$PROMPT_COMMAND; late_hook\"\n"
            "fi"
        )
        for bash in BASH_BINARIES:
            with self.subTest(bash=bash):
                self.check_prompt("", after=after, bash=bash)

    def test_resourcing_the_rc_keeps_one_copy_of_each_hook(self):
        assigns = (
            "PS1='FT> '\n"
            "theme_hook() { printf '<status:%d>' \"$?\"; PS1='THEME> '; }\n"
            "PROMPT_COMMAND='theme_hook'\n"
            + SOURCE_SNIPPETS
        )
        # Another tool prepends its hook after the block, once.
        prepends = (
            "PS1='FT> '\n"
            "theme_hook() { printf '<status:%d>' \"$?\"; PS1='THEME> '; }\n"
            + SOURCE_SNIPPETS
            + "[[ $PROMPT_COMMAND == *theme_hook* ]] || PROMPT_COMMAND=\"theme_hook;$PROMPT_COMMAND\"\n"
        )
        lines = [b'source "$FT_RC"', b'source "$FT_RC"', b"false",
                 b"declare -p PROMPT_COMMAND"]
        for bash in BASH_BINARIES:
            for name, rc_text in [("assigns", assigns), ("prepends", prepends)]:
                with self.subTest(bash=bash, rc=name):
                    *_, failed, listing = self.run_session(bash, rc_text, lines)
                    self.assertEqual(MARKER.findall(failed), [b"C", b"D;1", b"A", b"B"], failed)
                    self.assertIn(b"<status:1>", failed)
                    for hook in [b"__ft_precmd", b"__ft_finish_prompt", b"theme_hook"]:
                        self.assertEqual(listing.count(hook), 1, listing)


@unittest.skipUnless(shutil.which('zsh'), 'zsh unavailable')
class ZshPromptTests(unittest.TestCase):
    def test_resourcing_keeps_one_copy_of_each_hook(self):
        # Under KSH_ARRAYS a hook in the first slot has index 0, which an
        # index-based membership test reads as missing.
        for options in ["", "setopt ksharrays"]:
            with self.subTest(options=options):
                script = (
                    options + "\n"
                    'source "$1/osc7.sh"; source "$1/prompt.zsh"; source "$1/prompt.zsh"\n'
                    'print -r -- "${precmd_functions[@]}" "${preexec_functions[@]}"'
                )
                result = subprocess.run(
                    [shutil.which("zsh"), "-f", "-i", "-c", script, "zsh", str(SNIPPETS)],
                    capture_output=True, check=True, env={**os.environ, "TERM": "dumb"},
                )
                hooks = result.stdout.split()
                self.assertEqual(hooks, [b"__ft_precmd", b"__ft_zsh_prompt", b"__ft_preexec"], result)


if __name__ == "__main__":
    unittest.main()
