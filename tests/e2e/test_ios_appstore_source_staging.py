"""Exercise the actual packaging staging code against retained real files."""
import pathlib
import re
import subprocess
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[2] / 'scripts/package-ios-appstore.sh'
CODE = re.search(
    r"stage_source\(\) \{\npython3 .*?<<'PY'\n(.*?)\nPY\n\}",
    SCRIPT.read_text(), re.S,
).group(1)


class SourceStagingTests(unittest.TestCase):
    def setUp(self):
        self.base = pathlib.Path(tempfile.mkdtemp(prefix='ft-ios-stage-test-'))
        self.source = self.base / 'source'
        self.stage = self.base / 'stage'
        for package in ('FrankenTermSSH', 'FrankenTermTerminal'):
            (self.source / 'apps/apple' / package).mkdir(parents=True)
        (self.source / 'input.swift').write_text('let value = 42\n')
        (self.source / 'alias').symlink_to('input.swift')

    def run_stage(self, mode):
        return subprocess.run(
            ['python3', '-c', CODE, mode, str(self.source), str(self.stage),
             str(self.base / 'receipt.json')], capture_output=True, text=True, timeout=10,
        )

    def prepare(self):
        result = self.run_stage('prepare')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unchanged_and_generated_frameworks_pass(self):
        self.prepare()
        self.assertEqual(self.run_stage('verify').returncode, 0)
        for package in ('FrankenTermSSH', 'FrankenTermTerminal'):
            output = self.stage / 'apps/apple' / package / 'Artifacts/Test.xcframework'
            output.mkdir(parents=True)
            (output / 'library.a').write_bytes(b'generated library')
        self.assertEqual(self.run_stage('verify').returncode, 0)
        self.assertFalse((self.source / 'apps/apple/FrankenTermSSH/Artifacts').exists())

    def test_changed_input_fails(self):
        self.prepare()
        (self.stage / 'input.swift').write_text('changed source')
        self.assertNotEqual(self.run_stage('verify').returncode, 0)

    def test_new_source_fails(self):
        self.prepare()
        (self.stage / 'unexpected.swift').write_text('new source')
        self.assertNotEqual(self.run_stage('verify').returncode, 0)

    def test_changed_mode_fails(self):
        self.prepare()
        (self.stage / 'input.swift').chmod(0o700)
        self.assertNotEqual(self.run_stage('verify').returncode, 0)

    def test_retargeted_link_fails(self):
        self.prepare()
        (self.stage / 'alias').rename(self.base / 'retained-alias')
        (self.stage / 'alias').symlink_to('./input.swift')
        self.assertNotEqual(self.run_stage('verify').returncode, 0)

    def test_nested_stage_is_refused_before_copy(self):
        self.stage = self.source / 'nested'
        self.assertNotEqual(self.run_stage('prepare').returncode, 0)
        self.assertFalse(self.stage.exists())


if __name__ == '__main__':
    unittest.main()
