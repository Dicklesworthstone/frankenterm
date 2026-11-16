#!/usr/bin/env python3
"""Exercise guardian host family admission before any service is changed.

The real publication functions run with controlled version-reporting binaries.
These are compatibility and filesystem checks, not native guardian recovery or
sealed build-identity proof. Scratch directories are retained for diagnosis.
"""

import fcntl
import json
import os
import pathlib
import shlex
import signal
import subprocess
import tempfile
import time
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/mux-guardian-host.sh"
COMPONENTS = ("frankenterm-mux-server", "frankenterm-pty-guardian", "ft")
REVISION = "0123456789abcdef0123456789abcdef01234567"
OTHER_REVISION = "89abcdef0123456789abcdef0123456789abcdef"


class GuardianHostFamily(unittest.TestCase):
    def setUp(self):
        self.directory = pathlib.Path(tempfile.mkdtemp(prefix="ft-host-family-"))
        self.source = self.directory / "source"
        self.source.mkdir()
        self.base = self.directory / "published"
        self.env = os.environ.copy()
        self.env.update(
            FTG_BASE=str(self.base),
            FTG_STATE=str(self.directory / "state"),
            FTG_RUN=str(self.directory / "run"),
            FTG_CONFIG_DIR=str(self.directory / "config"),
            FTG_MUX_TOML=str(self.directory / "config/mux.toml"),
            FTG_MANAGER="direct",
            FTG_INSTANCE="family-test",
        )
        for component in COMPONENTS:
            self.write_version(component, self.version(component))

    @staticmethod
    def version(component, version="0.15.22", revision=REVISION):
        if component == "frankenterm-pty-guardian" or revision is None:
            return f"{component} {version}"
        return f"{component} {version} ({revision})"

    def write_version(self, component, text, status=0):
        path = self.source / component
        path.write_text(
            '#!/bin/sh\n[ "${1-}" = --version ] || exit 64\n'
            f"printf '%s\\n' {shlex.quote(text)}\nexit {status}\n"
        )
        path.chmod(0o755)

    def isolate_publication(self, label):
        self.base = self.directory / f"published-{label}"
        self.env["FTG_BASE"] = str(self.base)

    def publish(self):
        # Keep the production command-substitution context. Bash disables
        # errexit inside it, so probes and admission must propagate failures.
        source = SCRIPT.read_text()
        marker = "# ------------------------------------------------------------------- state ---"
        self.assertEqual(source.count(marker), 1)
        functions = source.split(marker, 1)[0]
        return subprocess.run(
            [
                "bash",
                "-c",
                functions
                + '\ndest=$(publish_family "$1") || exit $?\nprintf "%s\\n" "$dest"\n',
                str(SCRIPT),
                str(self.source),
            ],
            cwd=self.directory,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def assert_refused_before_publication(self, result):
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.base.exists(), "an invalid family must not be staged or published")
        self.assertEqual(result.stdout, "", "must not return an installed family")

    def assert_published(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        published = pathlib.Path(result.stdout.strip())
        self.assertEqual(published.parent, self.base / "families")
        self.assertTrue(published.is_dir())
        self.assertEqual({path.name for path in published.iterdir()}, set(COMPONENTS))
        for component in COMPONENTS:
            self.assertEqual(
                (published / component).read_bytes(),
                (self.source / component).read_bytes(),
            )
        return published

    def test_coherent_family_is_published_and_reused(self):
        published = self.assert_published(self.publish())
        self.assertEqual(published.name, "0.15.22-0123456")
        self.assertEqual(self.assert_published(self.publish()), published)

    def test_mismatched_cli_version_is_refused(self):
        self.write_version("ft", self.version("ft", version="0.15.21"))
        self.assert_refused_before_publication(self.publish())

    def test_mismatched_guardian_version_is_refused(self):
        self.write_version(
            "frankenterm-pty-guardian",
            self.version("frankenterm-pty-guardian", version="0.15.21"),
        )
        self.assert_refused_before_publication(self.publish())

    def test_mismatched_known_revisions_are_refused(self):
        self.write_version("ft", self.version("ft", revision=OTHER_REVISION))
        self.assert_refused_before_publication(self.publish())

    def test_matching_abbreviated_revision_is_accepted(self):
        self.write_version("ft", self.version("ft", revision=REVISION[:7]))
        self.assert_published(self.publish())

    def test_matching_abbreviated_mux_revision_is_accepted(self):
        self.write_version(
            "frankenterm-mux-server",
            self.version("frankenterm-mux-server", revision=REVISION[:7]),
        )
        self.assert_published(self.publish())

    def test_full_revisions_with_the_same_short_prefix_are_not_conflated(self):
        self.write_version("ft", self.version("ft", revision=REVISION[:-1] + "8"))
        self.assert_refused_before_publication(self.publish())

    def test_dirty_cli_revision_is_checked(self):
        self.write_version("ft", self.version("ft", revision=OTHER_REVISION + "+dirty"))
        self.assert_refused_before_publication(self.publish())

    def test_matching_dirty_cli_build_is_accepted(self):
        self.write_version("ft", self.version("ft", revision=REVISION + "+dirty"))
        self.assert_published(self.publish())

    def test_gitless_build_uses_a_content_tag(self):
        for component in ("frankenterm-mux-server", "ft"):
            self.write_version(component, self.version(component, revision="unknown"))
        published = self.assert_published(self.publish())
        self.assertRegex(published.name, r"^0[.]15[.]22-b[0-9a-f]{6}$")

    def test_an_unknown_cli_revision_is_not_a_claimed_mismatch(self):
        self.write_version("ft", self.version("ft", revision="unknown+dirty"))
        self.assert_published(self.publish())

    def test_package_version_only_build_is_accepted(self):
        for component in COMPONENTS:
            self.write_version(component, self.version(component, revision=None))
        published = self.assert_published(self.publish())
        self.assertRegex(published.name, r"^0[.]15[.]22-b[0-9a-f]{6}$")

    def test_semver_prerelease_and_build_metadata_are_accepted(self):
        for component in COMPONENTS:
            self.write_version(component, self.version(component, version="0.15.22-rc.1+test.2"))
        published = self.assert_published(self.publish())
        self.assertEqual(published.name, "0.15.22-rc.1+test.2-0123456")

    def test_hyphens_inside_semver_identifiers_are_accepted(self):
        version = "0.15.22-alpha--preview+ci--linux"
        for component in COMPONENTS:
            self.write_version(component, self.version(component, version=version))
        published = self.assert_published(self.publish())
        self.assertEqual(published.name, f"{version}-0123456")

    def test_each_failed_version_probe_is_refused_even_with_plausible_stdout(self):
        for component in COMPONENTS:
            with self.subTest(component=component):
                self.isolate_publication(component)
                self.write_version(component, self.version(component), status=17)
                self.assert_refused_before_publication(self.publish())
                self.write_version(component, self.version(component))

    def test_empty_version_output_is_not_family_identity(self):
        for component in COMPONENTS:
            self.write_version(component, "")
        self.assert_refused_before_publication(self.publish())

    def test_empty_cli_output_is_refused(self):
        self.write_version("ft", "")
        self.assert_refused_before_publication(self.publish())

    def test_other_program_in_a_component_slot_is_refused(self):
        for component in COMPONENTS:
            with self.subTest(component=component):
                self.isolate_publication(component)
                self.write_version(component, "unrelated 0.15.22")
                self.assert_refused_before_publication(self.publish())
                self.write_version(component, self.version(component))

    def test_multiline_version_output_is_refused(self):
        self.write_version("ft", self.version("ft") + "\nunexpected output")
        self.assert_refused_before_publication(self.publish())

    def test_invalid_version_or_revision_is_refused(self):
        for index, (version, revision) in enumerate(
            (("../escape", REVISION), ("0.15.22", "not-a-commit"))
        ):
            with self.subTest(version=version, revision=revision):
                self.isolate_publication(index)
                for component in COMPONENTS:
                    self.write_version(component, self.version(component, version, revision))
                self.assert_refused_before_publication(self.publish())


class GuardianHostOperations(unittest.TestCase):
    """Real command admission with an independently observed kernel lock."""

    def setUp(self):
        self.directory = pathlib.Path(tempfile.mkdtemp(prefix="ft-host-operations-"))
        self.base = self.directory / "base"
        self.state = self.directory / "state"
        self.source = self.directory / "candidate"
        self.installed = self.directory / "installed"
        self.bin = self.directory / "bin"
        for path in (self.base / "bin", self.state / "logs", self.source, self.installed, self.bin):
            path.mkdir(parents=True)
        for name in ("mux-current", "mux-previous", "guardian-family"):
            (self.base / name).symlink_to(self.installed, target_is_directory=True)
        (self.state / "manager").write_text("systemd\n")
        (self.base / "bin/mux-guardian-host.sh").write_text("original launcher\n")
        self.entered = self.directory / "probe-entered"
        self.release = self.directory / "probe-release"
        self.services = self.directory / "service-calls"
        self.lock = self.base / "host-operation.lock"
        self.env = dict(os.environ, FTG_BASE=str(self.base), FTG_STATE=str(self.state),
                        FTG_RUN=str(self.directory / "run"),
                        FTG_CONFIG_DIR=str(self.directory / "config"),
                        FTG_MUX_TOML=str(self.directory / "config/mux.toml"),
                        FTG_INSTANCE="operations-test", FTG_MANAGER="systemd",
                        FTG_OPERATION_LOCK_TIMEOUT="0",
                        FTG_TEST_PROBE_ENTERED=str(self.entered),
                        FTG_TEST_PROBE_RELEASE=str(self.release),
                        FTG_TEST_SERVICES=str(self.services),
                        PATH=f"{self.bin}:{os.environ['PATH']}")
        for component in COMPONENTS:
            for directory in (self.source, self.installed):
                self.executable(directory / component,
                    '#!/bin/sh\ncase "${1-}" in\n'
                    f'  --version) printf "%s\\n" {shlex.quote(GuardianHostFamily.version(component))} ;;\n'
                    '  probe) printf "ready panes=0\\n" ;;\n'
                    '  *) exit 64 ;;\nesac\n')
        self.executable(self.source / "frankenterm-mux-server",
            '#!/usr/bin/env python3\nimport os, pathlib, sys, time\n'
            'assert sys.argv[1:] == ["--version"]\n'
            'marker = os.environ.get("FTG_TEST_PROBE_ENTERED")\n'
            'if marker:\n'
            '    pathlib.Path(marker).write_text(str(os.getpid()))\n'
            '    deadline = time.monotonic() + 20\n'
            '    while not pathlib.Path(os.environ["FTG_TEST_PROBE_RELEASE"]).exists():\n'
            '        if time.monotonic() > deadline: sys.exit(18)\n'
            '        time.sleep(0.01)\n'
            'sys.exit(17)\n')
        for manager in ("systemctl", "launchctl"):
            self.executable(self.bin / manager,
                '#!/usr/bin/env python3\nimport os, pathlib, sys\n'
                'with pathlib.Path(os.environ["FTG_TEST_SERVICES"]).open("a") as f:\n'
                '    f.write(" ".join(sys.argv[1:]) + "\\n")\n'
                'sys.exit(1)\n')

    @staticmethod
    def executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def run_host(self, *args, env=None):
        return subprocess.run(["bash", str(SCRIPT), *args], cwd=self.directory,
                              env=env or self.env, capture_output=True, text=True, timeout=5)

    def start_upgrade(self):
        owner = subprocess.Popen(
            ["bash", str(SCRIPT), "upgrade", "--family", str(self.source)],
            cwd=self.directory, env=self.env, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, start_new_session=True)
        self.addCleanup(self.finish_upgrade, owner)
        deadline = time.monotonic() + 5
        while not self.entered.exists():
            if owner.poll() is not None or time.monotonic() >= deadline:
                self.fail(f"upgrade did not reach its version probe: {owner.communicate(timeout=1)}")
            time.sleep(0.01)
        return owner

    def finish_upgrade(self, owner):
        self.release.touch()
        owner.communicate(timeout=5)

    def assert_lock_released(self):
        with self.lock.open("a") as descriptor:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_mutating_commands_cannot_interleave_with_an_upgrade(self):
        owner = self.start_upgrade()
        first_probe = self.entered.read_text()
        commands = (
            ("upgrade", "--family", str(self.source)),
            ("upgrade", "--family", str(self.source), "--recover"),
            ("rollback",),
            ("install", "--family", str(self.source), "--manager", "systemd"),
            ("stop-all", "--i-accept-pane-loss"),
            ("load-units",),
        )
        for command in commands:
            with self.subTest(command=command):
                env = dict(self.env, FTG_MANAGER="launchd") if command[0] == "load-units" else self.env
                result = self.run_host(*command, env=env)
                self.assertEqual(result.returncode, 75, result.stderr)
                self.assertIn("operation_lock_busy", result.stderr)
        self.assertIsNone(owner.poll())
        self.assertEqual(self.entered.read_text(), first_probe, "competing probes must not run")
        self.assertFalse(self.services.exists(), "no competing service change may start")
        self.assertFalse((self.base / "families").exists(), "no competing family may publish")
        for name in ("mux-current", "mux-previous", "guardian-family"):
            self.assertEqual((self.base / name).readlink(), self.installed)
        self.assertEqual((self.base / "bin/mux-guardian-host.sh").read_text(), "original launcher\n")
        self.assertEqual((self.state / "manager").read_text(), "systemd\n")

    def test_failure_releases_lock_without_replacing_its_inode(self):
        owner = self.start_upgrade()
        inode = self.lock.stat().st_ino
        self.release.touch()
        _, errors = owner.communicate(timeout=5)
        self.assertNotEqual(owner.returncode, 0)
        self.assertIn("cannot read frankenterm-mux-server version", errors)
        self.assert_lock_released()
        result = self.run_host("upgrade", "--family", str(self.source))
        self.assertIn("cannot read frankenterm-mux-server version", result.stderr)
        self.assertNotIn("operation_lock_busy", result.stderr)
        self.assertEqual(self.lock.stat().st_ino, inode)
        self.assertEqual(self.lock.read_bytes(), b"")

    def test_termination_releases_the_owned_process_groups_lock(self):
        owner = self.start_upgrade()
        os.killpg(owner.pid, signal.SIGTERM)
        owner.communicate(timeout=5)
        self.assertNotEqual(owner.returncode, 0)
        self.assert_lock_released()

    def test_lock_wait_is_bounded(self):
        self.start_upgrade()
        started = time.monotonic()
        result = self.run_host("rollback", env=dict(self.env, FTG_OPERATION_LOCK_TIMEOUT="1"))
        elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertIn("waited 1s", result.stderr)
        self.assertGreaterEqual(elapsed, 0.9)
        self.assertLess(elapsed, 3)

    def test_waiter_can_acquire_when_the_operation_finishes(self):
        owner = self.start_upgrade()
        env = dict(self.env, FTG_OPERATION_LOCK_TIMEOUT="5")
        env.pop("FTG_TEST_PROBE_ENTERED")
        waiter = subprocess.Popen(["bash", str(SCRIPT), "upgrade", "--family", str(self.source)],
                                  cwd=self.directory, env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True)
        self.release.touch()
        owner.communicate(timeout=5)
        _, errors = waiter.communicate(timeout=5)
        self.assertIn("cannot read frankenterm-mux-server version", errors)
        self.assertNotIn("operation_lock_busy", errors)
        self.assert_lock_released()

    def test_other_instance_is_independent(self):
        self.start_upgrade()
        other = self.directory / "other-base"
        other.mkdir()
        (other / "mux-current").symlink_to(self.installed, target_is_directory=True)
        env = dict(self.env, FTG_BASE=str(other), FTG_STATE=str(self.directory / "other-state"))
        env.pop("FTG_TEST_PROBE_ENTERED")
        result = self.run_host("upgrade", "--family", str(self.source), env=env)
        self.assertIn("cannot read frankenterm-mux-server version", result.stderr)
        self.assertNotIn("operation_lock_busy", result.stderr)

    def test_same_base_with_different_state_still_shares_the_lock(self):
        self.start_upgrade()
        result = self.run_host("rollback", env=dict(self.env, FTG_STATE=str(self.directory / "other-state")))
        self.assertEqual(result.returncode, 75, result.stderr)

    def test_contender_does_not_read_install_mutated_settings_before_locking(self):
        self.start_upgrade()
        observed = self.directory / "manager-read"
        self.executable(self.bin / "cat",
            '#!/bin/sh\nif [ "${1-}" = "$FTG_STATE/manager" ]; then\n'
            f'  : > {shlex.quote(str(observed))}\nfi\nexec /bin/cat "$@"\n')
        env = dict(self.env)
        env.pop("FTG_MANAGER")
        for command in (("upgrade", "--family", str(self.source)), ("rollback",),
                        ("stop-all", "--i-accept-pane-loss"), ("load-units",)):
            with self.subTest(command=command):
                result = self.run_host(*command, env=env)
                self.assertEqual(result.returncode, 75, result.stderr)
                self.assertFalse(observed.exists(), "settings must be read after acquiring ownership")

    def test_status_remains_available_during_an_operation(self):
        self.start_upgrade()
        result = self.run_host("status")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("mux current:", result.stdout)
        self.assertNotIn("operation_lock_busy", result.stderr)

    def test_invalid_wait_and_unsafe_lock_are_refused_before_probes(self):
        for value in ("-1", "61", "1.5", "unlimited", "999999999999999999"):
            with self.subTest(timeout=value):
                result = self.run_host("upgrade", "--family", str(self.source),
                                       env=dict(self.env, FTG_OPERATION_LOCK_TIMEOUT=value))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("operation_lock_invalid", result.stderr)
                self.assertFalse(self.entered.exists())
        self.lock.rename(self.base / "original-lock")
        target = self.directory / "unrelated"
        target.write_text("preserve these bytes")
        self.lock.symlink_to(target)
        result = self.run_host("upgrade", "--family", str(self.source))
        self.assertIn("operation_lock_invalid", result.stderr)
        self.assertEqual(target.read_text(), "preserve these bytes")
        self.assertFalse(self.entered.exists())

    def test_direct_service_does_not_inherit_operation_ownership(self):
        # Use the actual service launcher. The service keeps every descriptor
        # it receives open, so an accidental inherited lease would remain held.
        service_ready = self.directory / "service-ready"
        service_release = self.directory / "service-release"
        self.addCleanup(service_release.touch)
        self.executable(self.base / "bin/mux-guardian-host.sh",
            '#!/usr/bin/env python3\nimport json, os, pathlib, time\n'
            'try:\n    os.fstat(9)\n    inherited = True\n'
            'except OSError:\n    inherited = False\n'
            f'pathlib.Path({str(service_ready)!r}).write_text(json.dumps({{"pid": os.getpid(), "inherited": inherited}}))\n'
            'deadline = time.monotonic() + 10\n'
            f'while not pathlib.Path({str(service_release)!r}).exists() and time.monotonic() < deadline:\n'
            '    time.sleep(0.01)\n')
        functions = SCRIPT.read_text().rsplit('case "${1:-}" in', 1)[0]
        result = subprocess.run(
            ["bash", "-c", functions + '\nacquire_operation_lock install\nMANAGER=direct\nsvc_start mux-server\n', str(SCRIPT)],
            cwd=self.directory, env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        deadline = time.monotonic() + 5
        while not service_ready.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(service_ready.exists())
        service = json.loads(service_ready.read_text())
        os.kill(service["pid"], 0)
        self.assertFalse(service["inherited"], "service entry must not retain descriptor 9")
        self.assert_lock_released()

    def install_inventory_fixture(self, reply='{"ok":true,"data":[]}'):
        self.inventory = self.directory / "inventory-response"
        self.inventory.write_text(reply)
        self.env.update(FTG_TEST_INVENTORY=str(self.inventory),
                        FTG_TEST_INVENTORY_STATUS="0", FTG_INVENTORY_TIMEOUT="2",
                        FTG_TEST_MANAGER_RESPONSE="ActiveState=active\nMainPID=123\n",
                        FTG_TEST_MANAGER_STATUS="0", FTG_TEST_BROKER_PID=str(os.getpid()))
        self.executable(self.source / "frankenterm-mux-server",
            '#!/bin/sh\n[ "${1-}" = --version ] || exit 64\n'
            f'printf "%s\\n" {shlex.quote(GuardianHostFamily.version("frankenterm-mux-server"))}\n')
        self.executable(self.installed / "ft",
            '#!/usr/bin/env python3\nimport os, pathlib, sys\n'
            'assert sys.argv[1:] == ["robot", "state"]\n'
            'sys.stdout.buffer.write(pathlib.Path(os.environ["FTG_TEST_INVENTORY"]).read_bytes())\n'
            'sys.exit(int(os.environ["FTG_TEST_INVENTORY_STATUS"]))\n')
        self.executable(self.installed / "frankenterm-pty-guardian",
            '#!/bin/sh\n[ "${1-}" = probe ] || exit 64\nprintf "ready panes=2\\n"\n')
        self.executable(self.bin / "systemctl",
            '#!/usr/bin/env python3\nimport os, pathlib, sys\n'
            'args = sys.argv[1:]\n'
            'with pathlib.Path(os.environ["FTG_TEST_SERVICES"]).open("a") as f:\n'
            '    f.write(" ".join(args) + "\\n")\n'
            'if args[:6] == ["--user", "show", "-p", "ActiveState", "-p", "MainPID"]:\n'
            '    print(os.environ["FTG_TEST_MANAGER_RESPONSE"], end="")\n'
            '    sys.exit(int(os.environ["FTG_TEST_MANAGER_STATUS"]))\n'
            'if args[:5] == ["--user", "show", "-p", "MainPID", "--value"]:\n'
            '    print(os.environ["FTG_TEST_BROKER_PID"])\n    sys.exit(0)\n'
            'if args[:3] == ["--user", "is-active", "--quiet"]: sys.exit(0)\n'
            'sys.exit(1)\n')
        self.executable(self.bin / "pgrep", '#!/bin/sh\n[ "$1" = -P ] || exit 2\nprintf "101\\n102\\n"\n')

    def call_host_function(self, function, env=None):
        # Exercise the production functions with real subprocess I/O. Only
        # their external binaries and manager responses are controlled.
        functions = SCRIPT.read_text().rsplit('case "${1:-}" in', 1)[0]
        return subprocess.run(["bash", "-c", functions + "\n" + function + "\n", str(SCRIPT)],
                              cwd=self.directory, env=env or self.env,
                              capture_output=True, text=True, timeout=5)

    def test_inventory_preserves_empty_and_exact_u64_identities(self):
        self.install_inventory_fixture()
        result = self.call_host_function("mux_pane_ids")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "\n")
        self.inventory.write_text('{"ok":true,"data":[{"pane_id":18446744073709551615},{"pane_id":0},{"pane_id":7}]}')
        result = self.call_host_function("mux_pane_ids")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "0 7 18446744073709551615\n")

    def test_inventory_rejects_error_malformed_and_ambiguous_identity_data(self):
        self.install_inventory_fixture()
        cases = ("", "{", "[]", "{}", '{"ok":false,"data":[]}',
                 '{"ok":true,"data":null}', '{"ok":true,"data":{}}',
                 '{"ok":true,"data":[null]}', '{"ok":true,"data":[{}]}',
                 '{"ok":true,"data":[{"pane_id":true}]}',
                 '{"ok":true,"data":[{"pane_id":1.0}]}',
                 '{"ok":true,"data":[{"pane_id":"1"}]}',
                 '{"ok":true,"data":[{"pane_id":-1}]}',
                 '{"ok":true,"data":[{"pane_id":18446744073709551616}]}',
                 '{"ok":true,"data":[{"pane_id":1},{"pane_id":1}]}',
                 '{"ok":false,"ok":true,"data":[]}',
                 '{"ok":true,"data":[{"pane_id":NaN}]}')
        for reply in cases:
            with self.subTest(reply=reply):
                self.inventory.write_text(reply)
                result = self.call_host_function("mux_pane_ids")
                self.assertEqual(result.returncode, 65, result.stderr)
                self.assertIn("mux_inventory_invalid", result.stderr)
                self.assertEqual(result.stdout, "")

    def test_failed_inventory_command_cannot_supply_a_plausible_empty_receipt(self):
        self.install_inventory_fixture()
        self.env["FTG_TEST_INVENTORY_STATUS"] = "17"
        result = self.call_host_function("mux_pane_ids")
        self.assertEqual(result.returncode, 69, result.stderr)
        self.assertIn("mux_inventory_unavailable", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_running_mux_with_unknown_inventory_is_never_recovered_or_restarted(self):
        self.install_inventory_fixture()
        for reply, status in (('{"ok":false,"error":"private-failure-detail"}', "0"),
                              ("malformed-private-output", "0"),
                              ('{"ok":true,"data":[]}', "17")):
            with self.subTest(reply=reply, status=status):
                self.inventory.write_text(reply)
                self.env["FTG_TEST_INVENTORY_STATUS"] = status
                result = self.run_host("upgrade", "--family", str(self.source), "--recover")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("cannot verify an empty or stopped mux", result.stderr)
                self.assertNotIn("private-", result.stderr)
                calls = self.services.read_text()
                self.assertNotIn("--user restart", calls)
                self.assertNotIn("--user reset-failed", calls)
                for name in ("mux-current", "mux-previous", "guardian-family"):
                    self.assertEqual((self.base / name).readlink(), self.installed)
                self.assertEqual((self.base / "bin/mux-guardian-host.sh").read_text(), "original launcher\n")
                self.assert_lock_released()

    def test_upgrade_rejects_failed_inventory_before_waiting_or_changing_services(self):
        self.install_inventory_fixture('{"ok":true,"data":[{"pane_id":7}]}')
        self.env["FTG_TEST_INVENTORY_STATUS"] = "17"
        result = self.run_host("upgrade", "--family", str(self.source))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot verify the running mux pane ids", result.stderr)
        self.assertNotIn("--user restart", self.services.read_text())
        self.assertEqual((self.base / "mux-current").readlink(), self.installed)

    def test_successfully_empty_inventory_allows_recovery_admission(self):
        self.install_inventory_fixture()
        result = self.run_host("upgrade", "--family", str(self.source), "--recover")
        # The controlled restart fails. Its invocation proves admission, not
        # a native restoration; the production failure path restores links.
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("starting mux", result.stderr)
        self.assertIn("--user restart", self.services.read_text())
        self.assertEqual((self.base / "mux-current").readlink(), self.installed)

    def test_confirmed_stopped_service_can_recover_without_a_mux_rpc(self):
        self.install_inventory_fixture()
        self.env.update(FTG_TEST_INVENTORY_STATUS="17",
                        FTG_TEST_MANAGER_RESPONSE="ActiveState=inactive\nMainPID=0\n")
        result = self.run_host("upgrade", "--family", str(self.source), "--recover")
        self.assertIn("service is confirmed stopped", result.stderr)
        self.assertIn("--user restart", self.services.read_text())
        self.assertEqual((self.base / "mux-current").readlink(), self.installed)

    def test_manager_errors_and_live_pid_are_not_positive_stopped_evidence(self):
        self.install_inventory_fixture()
        cases = (("ActiveState=inactive\nMainPID=0\n", "1"),
                 ("ActiveState=failed\nMainPID=123\n", "0"),
                 ("ActiveState=activating\nMainPID=0\n", "0"),
                 ("ActiveState=inactive\n", "0"), ("", "0"))
        for reply, status in cases:
            with self.subTest(reply=reply, status=status):
                self.env.update(FTG_TEST_MANAGER_RESPONSE=reply, FTG_TEST_MANAGER_STATUS=status)
                self.assertNotEqual(self.call_host_function("mux_service_is_stopped").returncode, 0)
        self.env.update(FTG_TEST_MANAGER_RESPONSE="MainPID=0\nActiveState=failed\n", FTG_TEST_MANAGER_STATUS="0")
        result = self.call_host_function("mux_service_is_stopped")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_direct_stopped_evidence_requires_an_exited_recorded_process(self):
        env = dict(self.env, FTG_MANAGER="direct")
        self.assertNotEqual(self.call_host_function("mux_service_is_stopped", env=env).returncode, 0)
        pid_file = self.state / "run.mux-server.pid"
        for value in (str(os.getpid()), "0" + str(os.getpid()), "invalid", "0"):
            with self.subTest(pid=value):
                pid_file.write_text(value)
                self.assertNotEqual(self.call_host_function("mux_service_is_stopped", env=env).returncode, 0)
        child = subprocess.Popen(["bash", "-c", "exit 0"])
        child.wait(timeout=5)
        pid_file.write_text(str(child.pid))
        result = self.call_host_function("mux_service_is_stopped", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_inventory_probe_deadline_kills_its_owned_descendants(self):
        self.install_inventory_fixture()
        child_pid = self.directory / "probe-descendant"
        self.executable(self.installed / "ft",
            '#!/usr/bin/env python3\nimport pathlib, subprocess, sys, time\n'
            'child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])\n'
            f'pathlib.Path({str(child_pid)!r}).write_text(str(child.pid))\n'
            'print("private-output-must-not-be-logged", flush=True)\n')
        started = time.monotonic()
        result = self.call_host_function("mux_pane_ids", env=dict(self.env, FTG_INVENTORY_TIMEOUT="1"))
        self.assertEqual(result.returncode, 69, result.stderr)
        self.assertIn("exceeded its deadline", result.stderr)
        self.assertNotIn("private-output", result.stderr)
        self.assertLess(time.monotonic() - started, 4)
        self.assertTrue(child_pid.exists())
        # A killed orphan can remain a zombie until the host's PID 1 reaps it.
        observed = subprocess.run(["ps", "-o", "stat=", "-p", child_pid.read_text()],
                                  capture_output=True, text=True, timeout=2)
        self.assertTrue(observed.returncode != 0 or observed.stdout.strip().startswith("Z"), observed.stdout)

    def test_inventory_response_budget_is_enforced_without_logging_content(self):
        self.install_inventory_fixture()
        self.executable(self.installed / "ft",
            '#!/usr/bin/env python3\nimport sys\nsys.stdout.buffer.write(b"x" * (16 * 1024 * 1024 + 1))\n')
        result = self.call_host_function("mux_pane_ids")
        self.assertEqual(result.returncode, 65, result.stderr)
        self.assertIn("response size limit", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_cancelled_inventory_owner_reaps_its_separate_probe_group(self):
        self.install_inventory_fixture()
        probe_pid = self.directory / "cancelled-probe"
        self.executable(self.installed / "ft",
            '#!/usr/bin/env python3\nimport os, pathlib, time\n'
            f'pathlib.Path({str(probe_pid)!r}).write_text(str(os.getpid()))\n'
            'time.sleep(30)\n')
        functions = SCRIPT.read_text().rsplit('case "${1:-}" in', 1)[0]
        owner = subprocess.Popen(["bash", "-c", functions + "\nmux_pane_ids\n", str(SCRIPT)],
                                 cwd=self.directory, env=self.env, start_new_session=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not probe_pid.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(probe_pid.exists())
            os.killpg(owner.pid, signal.SIGTERM)
            owner.communicate(timeout=5)
            self.assertNotEqual(owner.returncode, 0)
            # The detached probe is outside the owner's group. Only the
            # inventory reader's cancellation cleanup can terminate it.
            observed = subprocess.run(["ps", "-o", "stat=", "-p", probe_pid.read_text()],
                                      capture_output=True, text=True, timeout=2)
            self.assertTrue(observed.returncode != 0 or observed.stdout.strip().startswith("Z"), observed.stdout)
        finally:
            if owner.poll() is None:
                os.killpg(owner.pid, signal.SIGKILL)
                owner.communicate(timeout=5)
            if probe_pid.exists():
                try:
                    os.killpg(int(probe_pid.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    unittest.main(verbosity=2)
