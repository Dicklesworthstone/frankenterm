#!/usr/bin/env python3
"""Exercise the real host launcher with isolated service and binary adapters.

These tests prove host startup admission, not native guardian recovery. They
retain their scratch directories and never start or stop a user service.
"""

import json
import os
import pathlib
import subprocess
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/mux-guardian-host.sh"


class GuardianHostStartup(unittest.TestCase):
    def setUp(self):
        self.directory = pathlib.Path(tempfile.mkdtemp(prefix="ft-host-startup-"))
        self.base = self.directory / "family"
        self.state = self.directory / "state"
        self.recovery = self.state / "recovery"
        self.store = self.recovery / "store"
        self.bin = self.directory / "bin"
        for path in (
            self.base / "guardian-family",
            self.base / "mux-current",
            self.store / "roots",
            self.bin,
        ):
            path.mkdir(parents=True, exist_ok=True)
        self.args_path = self.directory / "mux-args.json"
        self.prepared_path = self.directory / "prepared-store"
        (self.store / "retained-object").write_text("uncommitted recovery object")
        (self.recovery / "ids.env").write_text(
            "FTG_NAMESPACE=namespace\nFTG_POLICY=policy\n"
            "FTG_ROOT_ID=root\nFTG_SESSION=session\n"
        )
        self.env = os.environ.copy()
        self.env.update(
            PATH=f"{self.bin}:{os.environ['PATH']}",
            FTG_INSTANCE="startup-test",
            FTG_BASE=str(self.base),
            FTG_STATE=str(self.state),
            FTG_RUN=str(self.directory / "run"),
            FTG_CONFIG_DIR=str(self.directory / "config"),
            FTG_MUX_TOML=str(self.directory / "config/mux.toml"),
            FTG_MANAGER="systemd",
            FTG_RECOVERY_INTERVAL="10",
            FTG_TEST_PANES="0",
            FTG_TEST_BROKER_PID=str(os.getpid()),
            FTG_TEST_CHILDREN="",
            FTG_TEST_CENSUS_STATUS="1",
            FTG_TEST_MUX_ARGS=str(self.args_path),
            FTG_TEST_PREPARED=str(self.prepared_path),
        )
        self.write_executable(
            self.base / "guardian-family/frankenterm-pty-guardian",
            '#!/bin/sh\n[ "$1" = probe ] || exit 2\n'
            'printf "ready panes=%s\\n" "$FTG_TEST_PANES"\n',
        )
        self.write_executable(
            self.base / "mux-current/frankenterm-mux-server",
            "#!/usr/bin/env python3\nimport json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['FTG_TEST_MUX_ARGS']).write_text("
            "json.dumps(sys.argv[1:]))\n",
        )
        self.write_executable(
            self.base / "guardian-family/ft",
            "#!/usr/bin/env python3\nimport os, pathlib, sys\n"
            "assert sys.argv[1:3] == ['snapshot', 'prepare-recovery-store']\n"
            "root = pathlib.Path(sys.argv[sys.argv.index('--root') + 1])\n"
            "(root / 'roots').mkdir(parents=True)\n"
            "pathlib.Path(os.environ['FTG_TEST_PREPARED']).write_text(str(root))\n",
        )
        self.write_executable(
            self.bin / "systemctl",
            '#!/bin/sh\ncase "$*" in\n'
            '  "--user show -p MainPID --value frankenterm-pty-broker-startup-test.service")\n'
            '    printf "%s\\n" "$FTG_TEST_BROKER_PID" ;;\n'
            '  *) exit 2 ;;\nesac\n',
        )
        self.write_executable(
            self.bin / "pgrep",
            '#!/bin/sh\n[ "$1" = -P ] || exit 2\n'
            '[ -z "$FTG_TEST_CHILDREN" ] || printf "%s\\n" "$FTG_TEST_CHILDREN"\n'
            'exit "$FTG_TEST_CENSUS_STATUS"\n',
        )

    @staticmethod
    def write_executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def add_root(self):
        (self.store / "roots/slot_a.root").write_text("committed root")

    def launch(self):
        return subprocess.run(
            ["bash", str(SCRIPT), "run-mux"],
            cwd=self.directory,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def assert_refused_without_mutation(self, result):
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.args_path.exists(), "mux must not run startup hooks")
        self.assertFalse(self.prepared_path.exists(), "must not prepare a fresh store")
        self.assertEqual(
            (self.store / "retained-object").read_text(),
            "uncommitted recovery object",
        )
        self.assertEqual(list(self.recovery.glob("store.retired-*")), [])

    def test_live_guardian_without_committed_root_never_starts_fresh(self):
        self.env["FTG_TEST_PANES"] = "12"
        result = self.launch()
        self.assert_refused_without_mutation(result)
        self.assertIn("no committed recovery snapshot", result.stderr)

    def test_live_guardian_restores_from_committed_root(self):
        self.env["FTG_TEST_PANES"] = "12"
        self.add_root()
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--recovery-restore", json.loads(self.args_path.read_text()))
        self.assertFalse(self.prepared_path.exists())
        self.assertTrue((self.store / "roots/slot_a.root").exists())

    def test_empty_guardian_with_broker_children_never_starts_fresh(self):
        self.env.update(FTG_TEST_CHILDREN="101\n102", FTG_TEST_CENSUS_STATUS="0")
        for has_root in (False, True):
            with self.subTest(committed_root=has_root):
                if has_root:
                    self.add_root()
                result = self.launch()
                self.assert_refused_without_mutation(result)
                self.assertIn("broker still owns", result.stderr)

    def test_unknown_broker_custody_does_not_retire_a_committed_root(self):
        self.add_root()
        cases = (
            ("0", "1"),
            ("invalid", "1"),
            (str(os.getpid()), "2"),
            (str(os.getpid()), "127"),
        )
        for broker_pid, census_status in cases:
            with self.subTest(broker_pid=broker_pid, census_status=census_status):
                self.env.update(
                    FTG_TEST_BROKER_PID=broker_pid,
                    FTG_TEST_CENSUS_STATUS=census_status,
                )
                result = self.launch()
                self.assert_refused_without_mutation(result)
                self.assertTrue((self.store / "roots/slot_a.root").exists())
                self.assertIn("cannot verify", result.stderr)

    def test_departed_broker_is_not_an_empty_broker(self):
        child = subprocess.Popen(["bash", "-c", "exit 0"])
        child.wait(timeout=10)
        self.env["FTG_TEST_BROKER_PID"] = str(child.pid)
        self.add_root()
        result = self.launch()
        self.assert_refused_without_mutation(result)
        self.assertIn("is not running", result.stderr)

    def test_broker_changed_during_census_cannot_authorize_fresh_start(self):
        marker = self.directory / "broker-observed"
        self.env["FTG_TEST_BROKER_OBSERVED"] = str(marker)
        self.write_executable(
            self.bin / "systemctl",
            '#!/bin/sh\nif [ -e "$FTG_TEST_BROKER_OBSERVED" ]; then\n'
            '  printf "0\\n"\nelse\n'
            '  printf "%s\\n" "$FTG_TEST_BROKER_PID"\n'
            '  : > "$FTG_TEST_BROKER_OBSERVED"\nfi\n',
        )
        self.add_root()
        result = self.launch()
        self.assert_refused_without_mutation(result)
        self.assertIn("changed or exited", result.stderr)

    def test_unknown_broker_custody_also_blocks_the_first_start(self):
        self.env["FTG_TEST_BROKER_PID"] = "0"
        result = self.launch()
        self.assert_refused_without_mutation(result)

    def test_failed_census_normalization_cannot_hide_broker_children(self):
        self.env.update(FTG_TEST_CHILDREN="101", FTG_TEST_CENSUS_STATUS="0")
        self.write_executable(self.bin / "sort", "#!/bin/sh\nexit 2\n")
        self.add_root()
        result = self.launch()
        self.assert_refused_without_mutation(result)

    def test_confirmed_empty_host_starts_fresh(self):
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("--recovery-restore", json.loads(self.args_path.read_text()))
        self.assertFalse(self.prepared_path.exists())
        self.assertTrue((self.store / "retained-object").exists())

    def test_confirmed_empty_host_preserves_old_store_before_fresh_start(self):
        self.add_root()
        result = self.launch()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("--recovery-restore", json.loads(self.args_path.read_text()))
        self.assertEqual(self.prepared_path.read_text(), str(self.store))
        retired = list(self.recovery.glob("store.retired-*"))
        self.assertEqual(len(retired), 1)
        self.assertEqual((retired[0] / "roots/slot_a.root").read_text(), "committed root")
        self.assertEqual(
            (retired[0] / "retained-object").read_text(),
            "uncommitted recovery object",
        )

    def test_missing_recovery_identity_never_launches_an_unguarded_mux(self):
        (self.recovery / "ids.env").rename(self.recovery / "ids.env.retained")
        result = self.launch()
        self.assert_refused_without_mutation(result)
        self.assertIn("recovery configuration", result.stderr)

    def test_status_reports_unknown_custody_instead_of_no_processes(self):
        self.env["FTG_TEST_BROKER_PID"] = "0"
        result = subprocess.run(
            ["bash", str(SCRIPT), "status"],
            cwd=self.directory,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pane processes:  unavailable", result.stdout)
        self.assertNotIn("pane processes:  []", result.stdout)


if __name__ == "__main__":
    unittest.main()
