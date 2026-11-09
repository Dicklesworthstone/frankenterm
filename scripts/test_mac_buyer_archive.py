"""Regression checks for the Finder-unlaunchable multi-root buyer archive."""
import importlib.util
import io
from pathlib import Path
import unittest
import zipfile

spec = importlib.util.spec_from_file_location(
    "buyer_package", Path(__file__).with_name("package-mac-buyer-download.py"))
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class BuyerArchiveTests(unittest.TestCase):
    def archive(self, extra=(), omitted=()):
        data = io.BytesIO()
        with zipfile.ZipFile(data, "w") as archive:
            names = ["FrankenTermExport.app/" + path for path in (
                "Contents/Info.plist", "Contents/MacOS/FrankenTermExport",
                "Contents/MacOS/frankenterm-machines")]
            for name in names + list(extra):
                if name not in omitted:
                    archive.writestr(name, b"fixture")
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


if __name__ == "__main__":
    unittest.main()
