#!/usr/bin/env python3
"""Run the installer's Linux census against retained, proc-shaped files.

The production shell function and its embedded Python run unchanged. Only the
proc root is supplied through the installer's source-only test seam; no user
processes are started, stopped, or renamed. This is census behavior coverage,
not native process-lifecycle or activation qualification.
"""

import os
import pathlib
import subprocess
import sys
import tempfile
import unittest


INSTALLER = pathlib.Path(__file__).resolve().parents[2] / "install.sh"


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux /proc census")
class InstallerProcessOwnership(unittest.TestCase):
    def setUp(self):
        # Keep the stock executable's path outside any FrankenTerm checkout or
        # directory named for the product. Retain every fixture for diagnosis.
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="ft-census-", dir="/tmp"))
        self.proc = self.root / "proc"
        self.proc.mkdir()
        self.destination = self.root / "destination"
        self.destination.mkdir()
        self.next_pid = 100

    def add_process(self, executable, *, comm=None, invalid_exe=False):
        process = self.proc / str(self.next_pid)
        self.next_pid += 1
        process.mkdir()
        name = pathlib.Path(executable).name
        (process / "comm").write_text((comm or name[:15]) + "\n")
        if invalid_exe:
            # readlink fails with OSError, like inaccessible exe metadata.
            (process / "exe").mkdir()
        else:
            target = pathlib.Path(executable)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("#!/bin/sh\nexit 0\n")
            target.chmod(0o755)
            (process / "exe").symlink_to(target)

    def environment(self):
        env = os.environ.copy()
        env.pop("FT_INSTALL_TEST_MUX_OWNERSHIP_STATE", None)
        env.pop("FT_INSTALL_TEST_FORCE_PS_CENSUS", None)
        env.update(
            FT_INSTALL_TEST_LIBRARY_ONLY="1",
            FT_INSTALL_TEST_ENABLE_RESOURCE_OVERRIDES="1",
            FT_INSTALL_TEST_PROC_ROOT=str(self.proc),
            DEST=str(self.destination),
        )
        return env

    def census(self):
        result = subprocess.run(
            [
                "bash", "-c",
                'source "$1"; installer_mux_ownership_state',
                "installer-census-test", str(INSTALLER),
            ],
            env=self.environment(), capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def test_stock_wezterm_does_not_block_activation(self):
        self.add_process(self.root / "stock" / "wezterm-gui")
        self.add_process(self.root / "stock" / "wezterm-mux-server")
        self.assertEqual(self.census(), "inactive")

    def test_frankenterm_process_blocks_activation(self):
        self.add_process(self.root / "owned" / "frankenterm-gui")
        self.assertEqual(self.census(), "active")

    def test_pre_rebrand_process_under_destination_blocks_activation(self):
        self.add_process(self.destination / "wezterm-mux-server")
        self.assertEqual(self.census(), "active")

    def test_pre_rebrand_process_under_frankenterm_path_blocks_activation(self):
        self.add_process(self.root / "FrankenTerm" / "wezterm-gui")
        self.assertEqual(self.census(), "active")

    def test_truncated_executable_identity_remains_ambiguous(self):
        self.add_process(self.root / "unknown" / "frankenterm-mux")
        self.assertEqual(self.census(), "ambiguous")

    def test_recognized_name_with_unknown_executable_remains_ambiguous(self):
        self.add_process(self.root / "unknown" / "renamed-gui", comm="frankenterm-gui")
        self.assertEqual(self.census(), "ambiguous")

    def test_unreadable_executable_identity_remains_ambiguous(self):
        self.add_process(self.root / "unknown" / "wezterm-gui", invalid_exe=True)
        self.assertEqual(self.census(), "ambiguous")

    def test_stock_process_does_not_clear_an_ambiguous_process(self):
        self.add_process(self.root / "unknown" / "frankenterm-mux")
        self.add_process(self.root / "stock" / "wezterm-gui")
        self.assertEqual(self.census(), "ambiguous")

    def test_active_process_takes_precedence_over_ambiguity(self):
        self.add_process(self.root / "unknown" / "frankenterm-mux")
        self.add_process(self.root / "owned" / "frankenterm-pty-guardian")
        self.assertEqual(self.census(), "active")

    def test_unrelated_executable_metadata_is_not_needed(self):
        self.add_process(self.root / "unrelated" / "sleep", invalid_exe=True)
        self.assertEqual(self.census(), "inactive")

    def test_unavailable_proc_root_remains_ambiguous(self):
        self.proc = self.root / "missing-proc"
        self.assertEqual(self.census(), "ambiguous")

    def test_executing_installer_rejects_the_source_only_override(self):
        result = subprocess.run(
            ["bash", str(INSTALLER), "--help"],
            env=self.environment(), capture_output=True, text=True, timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires sourcing from an isolated test shell", result.stderr)


@unittest.skipUnless(os.name == "posix", "POSIX installer process probes")
class InstallerCandidateVerification(unittest.TestCase):
    """Exercise the actual --verify block against owned candidate processes.

    ft-xxfwy.4: a pending candidate must pass the same bounded health contract
    as an activated generation, without accidentally executing the old DEST.
    These are installer behavior tests, not signed-release qualification.
    """

    def setUp(self):
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="ft-verify-", dir="/tmp"))
        self.destination = self.root / "old destination"
        self.candidate = self.root / "pending candidate"
        self.destination.mkdir()
        self.candidate.mkdir()
        self.log = self.root / "candidate-calls.log"
        self.child_pid = self.root / "child.pid"
        self.report = self.root / "ft-doctor.json"
        source = INSTALLER.read_text()
        start = source.index('if [ "$VERIFY" -eq 1 ]; then')
        end = source.index("\n# The candidate manifest came", start)
        self.verify_block = source[start:end]
        program = "#!" + sys.executable + "\n" + r'''
import os, pathlib, subprocess, sys, time

role = pathlib.Path(sys.argv[0]).name
mode = os.environ["FT_INSTALL_TEST_VERIFY_MODE"]
with open(os.environ["FT_INSTALL_TEST_VERIFY_LOG"], "a") as log:
    log.write(role + " " + " ".join(sys.argv[1:]) + "\n")
if sys.argv[1:] == ["--version"]:
    if mode == "bad-mux-version" and role == "frankenterm-mux-server":
        raise SystemExit(7)
    if mode == "hang-version" and role == "ft":
        time.sleep(60)
    print(role + " 0.0.0-test")
elif role == "ft" and sys.argv[1:] == ["doctor", "--json"]:
    if mode == "nonzero":
        print('{"ok":true}')
        raise SystemExit(9)
    elif mode == "hang":
        time.sleep(60)
    elif mode == "inherited-pipe":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        pathlib.Path(os.environ["FT_INSTALL_TEST_VERIFY_CHILD"]).write_text(str(child.pid))
    elif mode == "oversized":
        print('{"ok":true,"detail":"' + "x" * (1024 * 1024) + '"}')
        raise SystemExit(0)
    elif mode == "stderr":
        sys.stderr.write("private candidate diagnostic\n" * 100_000)
    elif mode == "stdin":
        if sys.stdin.read():
            raise SystemExit(10)
    reports = {
        "not-ok": '{"ok":false}', "null": 'null', "array": '[]',
        "missing-ok": '{}', "truthy-ok": '{"ok":1}',
        "malformed": '{"ok":true', "trailing": '{"ok":true} {"ok":true}',
    }
    print(reports.get(mode, '{"ok":true,"candidate":"pending"}'))
else:
    raise SystemExit(11)
'''
        for name in ("ft", "frankenterm-mux-server", "frankenterm-pty-guardian"):
            executable = self.candidate / name
            executable.write_text(program)
            executable.chmod(0o755)
            old = self.destination / name
            old.write_text("#!/bin/sh\necho old-generation-must-not-run >&2\nexit 99\n")
            old.chmod(0o755)

    def environment(self, mode):
        env = os.environ.copy()
        env.pop("FT_INSTALL_TEST_ACTIVATION_READINESS_FAIL", None)
        env.pop("FT_INSTALL_TEST_PROBE_TIMEOUT_SECONDS", None)
        env.update(
            FT_INSTALL_TEST_LIBRARY_ONLY="1",
            FT_INSTALL_TEST_ENABLE_RESOURCE_OVERRIDES="1",
            FT_INSTALL_TEST_VERIFY_MODE=mode,
            FT_INSTALL_TEST_VERIFY_LOG=str(self.log),
            FT_INSTALL_TEST_VERIFY_CHILD=str(self.child_pid),
        )
        return env

    def verify(self, mode="healthy", pending=True):
        script = '''source "$1"
DEST="$2"
PUBLISHED_PROCESS_FAMILY_ROOT="$3"
TMP="$4"
PENDING_PROCESS_FAMILY_GENERATION="$5"
VERIFY=1
QUIET=1
HAS_GUM=0
''' + self.verify_block
        return subprocess.run(
            ["bash", "-c", script, "installer-verify-test", str(INSTALLER),
             str(self.destination), str(self.candidate), str(self.root),
             "pending-generation" if pending else ""],
            env=self.environment(mode), input="unconsumed installer input\n",
            capture_output=True, text=True, timeout=20,
        )

    def test_healthy_pending_candidate_probes_every_role_by_immutable_path(self):
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.report.read_text(), '{"ok":true,"candidate":"pending"}\n')
        self.assertEqual(self.log.read_text().splitlines(), [
            "ft --version", "frankenterm-mux-server --version",
            "frankenterm-pty-guardian --version", "ft doctor --json",
        ])
        self.assertNotIn("old-generation", result.stderr)

    def test_activated_candidate_still_uses_immutable_path(self):
        result = self.verify(pending=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"candidate":"pending"', self.report.read_text())

    def test_unsuccessful_and_nonobject_health_reports_are_rejected(self):
        for mode in ("not-ok", "null", "array", "missing-ok", "truthy-ok",
                     "malformed", "trailing", "nonzero"):
            with self.subTest(mode=mode):
                result = self.verify(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("installation verification did not pass", result.stderr)
                self.assertEqual(self.report.read_bytes(), b"", "failed payload must not be published")

    def test_unhealthy_mux_prevents_the_doctor_probe(self):
        result = self.verify("bad-mux-version")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("doctor --json", self.log.read_text())
        self.assertEqual(self.report.read_bytes(), b"")

    def test_silent_and_version_hangs_respect_the_shared_deadline(self):
        for mode in ("hang", "hang-version"):
            with self.subTest(mode=mode):
                # Use the production 10-second budget, with a 20-second outer
                # watchdog and 60-second fixture. This checks cancellation,
                # not Python-startup performance on a shared, busy test host.
                result = self.verify(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.report.read_bytes(), b"")

    def test_child_holding_stdout_cannot_keep_the_installer_waiting(self):
        result = self.verify("inherited-pipe")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.child_pid.exists(), "the owned descendant must actually start")
        self.assertEqual(self.report.read_bytes(), b"")

    def test_output_budget_is_enforced_before_any_report_is_written(self):
        result = self.verify("oversized")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.report.read_bytes(), b"")
        self.assertLess(len(result.stderr), 1024)

    def test_noisy_stderr_and_inherited_stdin_do_not_escape_probe_ownership(self):
        for mode in ("stderr", "stdin"):
            with self.subTest(mode=mode):
                result = self.verify(mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, "")
                self.assertLess(self.report.stat().st_size, 1024)

    def test_activation_default_remains_silent_and_probes_dest(self):
        result = subprocess.run(
            ["bash", "-c", 'source "$1"; DEST="$2"; probe_activated_process_family',
             "installer-probe-test", str(INSTALLER), str(self.candidate)],
            env=self.environment("healthy"), capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertIn("ft doctor --json", self.log.read_text())


if __name__ == "__main__":
    unittest.main()
