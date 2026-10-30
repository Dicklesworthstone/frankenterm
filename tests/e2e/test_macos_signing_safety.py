#!/usr/bin/env python3
"""Exercise refusal paths against real files; does not mock Apple acceptance."""
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / 'scripts/sign-macos-bundle.py'


class SigningSafety(unittest.TestCase):
    def test_prior_notarization_evidence_prevents_resigning(self):
        for suffix in ('.notarization.zip', '.notarization.json'):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as directory:
                app = pathlib.Path(directory) / 'Retained.app'
                app.mkdir()
                retained = app / 'retained-artifact'
                retained.write_bytes(b'previously qualified bytes')
                receipt = app.with_suffix(suffix)
                receipt.write_bytes(b'existing evidence')
                result = subprocess.run([
                    'python3', str(SCRIPT), str(app),
                    'Developer ID Application: Test Identity', '--notarize',
                ], capture_output=True, text=True, timeout=30)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('retain prior notarization evidence', result.stderr)
                self.assertEqual(retained.read_bytes(), b'previously qualified bytes')
                self.assertEqual(receipt.read_bytes(), b'existing evidence')
                self.assertEqual(sorted(p.name for p in app.iterdir()), ['retained-artifact'])

    def test_ad_hoc_identity_is_not_distribution_signing(self):
        result = subprocess.run([
            'python3', str(SCRIPT), '/nonexistent/FrankenTerm.app', '-',
        ], capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('explicit Developer ID Application identity is required', result.stderr)


if __name__ == '__main__':
    unittest.main()
