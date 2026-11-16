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


if __name__ == "__main__":
    unittest.main()
