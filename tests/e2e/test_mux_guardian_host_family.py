#!/usr/bin/env python3
"""Exercise guardian host family admission before any service is changed.

The real publication functions run with controlled version-reporting binaries.
These are compatibility and filesystem checks, not native guardian recovery or
sealed build-identity proof. Scratch directories are retained for diagnosis.
"""

import os
import pathlib
import shlex
import subprocess
import tempfile
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
