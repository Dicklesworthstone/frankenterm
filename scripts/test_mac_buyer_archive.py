"""Regression checks for the Finder-unlaunchable multi-root buyer archive."""
import importlib.util
import io
from pathlib import Path
import plistlib
import stat
import subprocess
import sys
import tempfile
import unittest
import warnings
import zipfile

spec = importlib.util.spec_from_file_location(
    "buyer_package", Path(__file__).with_name("package-mac-buyer-download.py"))
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class BuyerArchiveTests(unittest.TestCase):
    def archive(self, extra=(), omitted=(), overrides=None, modes=None, info=None):
        data = io.BytesIO()
        contents = {
            "FrankenTermExport.app/Contents/Info.plist": plistlib.dumps(info if info is not None else {
                "CFBundleIdentifier": "org.frankenterm.offline-export",
                "CFBundleExecutable": "FrankenTermExport",
                "CFBundlePackageType": "APPL",
            }),
            "FrankenTermExport.app/Contents/MacOS/FrankenTermExport": b"executable fixture",
            "FrankenTermExport.app/Contents/MacOS/frankenterm-machines": b"helper fixture",
        }
        contents.update(overrides or {})
        with zipfile.ZipFile(data, "w") as archive:
            for name in list(contents) + list(extra):
                if name not in omitted:
                    entry = zipfile.ZipInfo(name)
                    entry.create_system = 3
                    mode = stat.S_IFDIR | 0o755 if name.endswith("/") else stat.S_IFREG | 0o755
                    entry.external_attr = (modes or {}).get(name, mode) << 16
                    archive.writestr(entry, contents.get(name, b"fixture"))
        data.seek(0)
        return data

    def test_single_app_with_resource_metadata(self):
        package.verify_archive_layout(self.archive(("__MACOSX/._FrankenTermExport.app",)))

    def test_rejects_shipped_multi_root_shape(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive((
                "FrankenTerm.app/Contents/Info.plist",
                "FrankenTerm.app.component-manifest.json")))

    def test_rejects_fake_app_wrapper(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive((
                "FrankenTerm-darwin-arm64.app/FrankenTermExport.app/Contents/Info.plist",)))

    def test_rejects_missing_export_executable(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(omitted=(
                "FrankenTermExport.app/Contents/MacOS/FrankenTermExport",)))

    def test_rejects_missing_machine_helper(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(omitted=(
                "FrankenTermExport.app/Contents/MacOS/frankenterm-machines",)))

    def test_rejects_unsafe_paths_including_resource_metadata(self):
        for name in ("FrankenTermExport.app/../outside", "/outside",
                     "FrankenTermExport.app//Contents/file",
                     "FrankenTermExport.app/Contents/./file",
                     "FrankenTermExport.app/Contents\\file",
                     "__MACOSX/../outside"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                package.verify_archive_layout(self.archive((name,)))

    def test_rejects_duplicate_and_case_colliding_entries(self):
        for name in ("FrankenTermExport.app/Contents/Info.plist",
                     "FrankenTermExport.app/Contents/INFO.plist"):
            with self.subTest(name=name), warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                with self.assertRaises(ValueError):
                    package.verify_archive_layout(self.archive((name,)))

    def test_rejects_unicode_collisions(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive((
                "FrankenTermExport.app/Contents/é", "FrankenTermExport.app/Contents/e\u0301")))

    def test_rejects_arbitrary_metadata_payload(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(("__MACOSX/Other.app/._file",)))

    def test_rejects_symlink_and_nonexecutable_launcher(self):
        name = "FrankenTermExport.app/Contents/MacOS/FrankenTermExport"
        for mode in (stat.S_IFLNK | 0o777, stat.S_IFREG | 0o644):
            with self.subTest(mode=mode), self.assertRaises(ValueError):
                package.verify_archive_layout(self.archive(modes={name: mode}))

    def test_rejects_empty_launcher(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(overrides={
                "FrankenTermExport.app/Contents/MacOS/FrankenTermExport": b""}))

    def test_rejects_wrong_bundle_metadata(self):
        valid = {"CFBundleIdentifier": "org.frankenterm.offline-export",
                 "CFBundleExecutable": "FrankenTermExport", "CFBundlePackageType": "APPL"}
        cases = [{}, []] + [dict(valid, **{key: "Wrong"}) for key in valid]
        for info in cases:
            with self.subTest(info=info), self.assertRaises(ValueError):
                package.verify_archive_layout(self.archive(info=info))

    def test_rejects_file_used_as_directory(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(("FrankenTermExport.app/Contents",)))

    def test_rejects_corrupt_resource_bytes(self):
        data = self.archive(overrides={"FrankenTermExport.app/Contents/Resources/test": b"unique resource bytes"})
        damaged = data.getvalue().replace(b"unique resource bytes", b"broken resource bytes", 1)
        with self.assertRaises(ValueError):
            package.verify_archive_layout(io.BytesIO(damaged))

    def test_rejects_nonexecutable_helper(self):
        with self.assertRaises(ValueError):
            package.verify_archive_layout(self.archive(modes={
                "FrankenTermExport.app/Contents/MacOS/frankenterm-machines": stat.S_IFREG | 0o644}))

    def test_missing_source_creates_no_archive(self):
        work = Path(tempfile.mkdtemp(prefix="ft-export-missing-test-"))
        output = work / "export.zip"
        with self.assertRaises(ValueError):
            package.write_export_archive(work / "FrankenTermExport.app", output)
        self.assertFalse(output.exists())

    @unittest.skipUnless(sys.platform == "darwin", "Exercises macOS ditto")
    def test_real_production_archive_command_round_trip(self):
        # Retain test artifacts for inspection; never delete user/build data.
        work = Path(tempfile.mkdtemp(prefix="ft-export-archive-test-"))
        fixture = self.archive()
        with zipfile.ZipFile(fixture) as zipped:
            for entry in zipped.infolist():
                path = work / entry.filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(zipped.read(entry))
                path.chmod(stat.S_IMODE(entry.external_attr >> 16))
        archive = work / "FrankenTerm-darwin-arm64.app.zip"
        package.write_export_archive(work / "FrankenTermExport.app", archive)
        subprocess.run(["ditto", "-x", "-k", str(archive), str(work / "extracted")],
                       check=True, timeout=60)
        extracted = work / "extracted"
        self.assertEqual([p.name for p in extracted.iterdir()], ["FrankenTermExport.app"])
        self.assertEqual((extracted / "FrankenTermExport.app/Contents/MacOS/FrankenTermExport").read_bytes(),
                         b"executable fixture")
        original_bytes = archive.read_bytes()
        with self.assertRaises(ValueError):
            package.write_export_archive(work / "FrankenTermExport.app", archive)
        self.assertEqual(archive.read_bytes(), original_bytes)


if __name__ == "__main__":
    unittest.main()
