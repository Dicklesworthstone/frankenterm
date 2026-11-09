"""Build and package the standalone notarized Export utility for iOS buyers."""

import argparse
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import tempfile
import zipfile
import unicodedata


def verify_archive_layout(archive):
    """Archive Utility must extract one real app, never a misleading .app folder."""
    with zipfile.ZipFile(archive) as zipped:
        entries = zipped.infolist()
        seen = set()
        files = set()
        for entry in entries:
            name = entry.filename
            parts = name.rstrip("/").split("/")
            if any(part in ("", ".", "..") for part in parts) or "\\" in name:
                raise ValueError(f"Unsafe archive path: {name}")
            # Finder commonly extracts onto case-insensitive, normalization-
            # insensitive APFS. Reject collisions before extraction loses data.
            canonical = unicodedata.normalize("NFD", name.rstrip("/")).casefold()
            if canonical in seen:
                raise ValueError(f"Duplicate archive path: {name}")
            seen.add(canonical)
            mode = entry.external_attr >> 16
            expected_type = stat.S_IFDIR if entry.is_dir() else stat.S_IFREG
            if stat.S_IFMT(mode) != expected_type:
                raise ValueError(f"Unsupported archive entry type: {name}")
            if not entry.is_dir():
                files.add(canonical)
            if parts[0] == "__MACOSX":
                if entry.is_dir():
                    if len(parts) > 1 and parts[1] != "FrankenTermExport.app":
                        raise ValueError(f"Unrelated resource metadata: {name}")
                elif not (parts[-1].startswith("._") and
                          (parts[1:] == ["._FrankenTermExport.app"] or
                           (len(parts) > 2 and parts[1] == "FrankenTermExport.app"))):
                    raise ValueError(f"Unrelated resource metadata: {name}")
        for name in seen:
            parts = name.split("/")
            if any("/".join(parts[:i]) in files for i in range(1, len(parts))):
                raise ValueError(f"Archive file is also used as a directory: {name}")
        names = {entry.filename for entry in entries}
        payload = {name for name in names if not name.startswith("__MACOSX/")}
        if not payload or any(
            not name.startswith("FrankenTermExport.app/") for name in payload
        ):
            raise ValueError("Buyer ZIP must contain only the standalone Export app")
        for required in ("Contents/Info.plist", "Contents/MacOS/FrankenTermExport",
                         "Contents/MacOS/frankenterm-machines"):
            if "FrankenTermExport.app/" + required not in payload:
                raise ValueError(f"Buyer ZIP is missing {required}")
            entry = zipped.getinfo("FrankenTermExport.app/" + required)
            if entry.file_size == 0:
                raise ValueError(f"Buyer ZIP has empty {required}")
            if required.startswith("Contents/MacOS/") and not (entry.external_attr >> 16) & stat.S_IXUSR:
                raise ValueError(f"Buyer ZIP has non-executable {required}")
        info = plistlib.loads(zipped.read("FrankenTermExport.app/Contents/Info.plist"))
        if not isinstance(info, dict) or any(info.get(key) != value for key, value in {
            "CFBundleIdentifier": "org.frankenterm.offline-export",
            "CFBundleExecutable": "FrankenTermExport",
            "CFBundlePackageType": "APPL",
        }.items()):
            raise ValueError("Buyer ZIP has incorrect app identity or executable metadata")
        corrupt = zipped.testzip()
        if corrupt is not None:
            raise ValueError(f"Buyer ZIP has corrupt contents: {corrupt}")


def write_export_archive(export, archive):
    """Keep the actual production archiving command covered by integration tests."""
    if export.name != "FrankenTermExport.app" or export.is_symlink() or not export.is_dir():
        raise ValueError("Expected an existing standalone Export app directory")
    if archive.exists() or archive.is_symlink():
        raise ValueError("Refusing to replace an existing archive")
    run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", export, archive)
    verify_archive_layout(archive)


def run(*args):
    # ubs:ignore python.taint.command -- fixed tool names, argv operands, no shell evaluation.
    subprocess.run(args, check=True, timeout=7200, shell=False)


def verify_app(app, bundle_id, team):
    with (app / "Contents/Info.plist").open("rb") as f:
        if plistlib.load(f)["CFBundleIdentifier"] != bundle_id:
            raise ValueError(f"Unexpected bundle ID in {app}")
    run("codesign", "--verify", "--deep", "--strict", "-R",
        f'=anchor apple generic and certificate leaf[subject.OU] = "{team}"', app)
    run("xcrun", "stapler", "validate", app)
    run("spctl", "--assess", "--type", "execute", app)


def build_export(source, work, helper):
    # The shared project declares iOS-only binary packages. Xcode resolves
    # those even for the Mac scheme. Project only the existing Export target
    # into private staging, preserving its build phases, entitlements and pins.
    apple = work / "apps/apple"
    original = source / "apps/apple/FrankenTerm.xcodeproj"
    project = apple / "FrankenTerm.xcodeproj"
    project.mkdir(parents=True)
    doc = plistlib.loads(subprocess.check_output(
        ["plutil", "-convert", "xml1", "-o", "-", original / "project.pbxproj"], timeout=60))
    objects = doc["objects"]
    root = objects[doc["rootObject"]]
    targets = [t for t in root["targets"] if objects[t]["name"] == "FrankenTermExport"]
    packages = [p for p in root["packageReferences"] if objects[p]["relativePath"] == "FrankenTermKit"]
    if len(targets) != 1 or len(packages) != 1:
        raise ValueError("Cannot identify the Export target and Kit package")
    target = objects[targets[0]]
    if target["dependencies"] or [objects[p]["productName"] for p in target["packageProductDependencies"]] != ["FrankenTermKit"]:
        raise ValueError("Export dependencies changed. Update its release inputs.")
    root.update(targets=targets, packageReferences=packages)
    with (project / "project.pbxproj").open("xb") as f:
        plistlib.dump(doc, f)
    for relative in ("project.xcworkspace/xcshareddata/swiftpm/Package.resolved",
                     "xcshareddata/xcschemes/FrankenTermExport.xcscheme"):
        (project / relative).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(original / relative, project / relative)
    shutil.copytree(source / "apps/apple/FrankenTermKit", apple / "FrankenTermKit",
                    ignore=shutil.ignore_patterns(".build", ".swiftpm"))
    companion = work / "apps/macos/FrankenTermExport"
    shutil.copytree(source / "apps/macos/FrankenTermExport", companion,
                    ignore=shutil.ignore_patterns("Artifacts"))
    (companion / "Artifacts").mkdir()
    shutil.copy2(helper, companion / "Artifacts/frankenterm-machines")
    run("xcodebuild", "-project", project, "-scheme", "FrankenTermExport",
        "-configuration", "Release", "-destination", "generic/platform=macOS",
        "-jobs", "2", "-onlyUsePackageVersionsFromResolvedFile",
        "-derivedDataPath", work / "DerivedData", "-clonedSourcePackagesDirPath", work / "Packages",
        "ARCHS=arm64", "CODE_SIGN_IDENTITY=-", "CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO", "build")
    app = work / "DerivedData/Build/Products/Release/FrankenTermExport.app"
    entitlements = plistlib.loads(subprocess.check_output(
        ["codesign", "--display", "--entitlements", "-", "--xml", app], timeout=60))
    with (companion / "FrankenTermExport.entitlements").open("rb") as f:
        if entitlements != plistlib.load(f) or entitlements.get("com.apple.security.get-task-allow"):
            raise ValueError("Export must retain its declared capabilities without debugger access")
    # Xcode signs the helper during the copy phase, so its signed bytes can differ.
    run("lipo", app / "Contents/MacOS/frankenterm-machines", "-verify_arch", "arm64")
    return app


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--helper", type=Path, required=True)
    parser.add_argument("--zip", type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).resolve().parents[1]
    if (source / ".git").exists() or not os.environ.get("DSR_RELEASE_GIT_SHA"):
        raise ValueError("Use the immutable DSR release source export")
    desktop, helper, archive = args.desktop.resolve(), args.helper.resolve(), args.zip.absolute()
    identity = os.environ["FT_MAC_DEVELOPER_ID"]
    team = identity.rsplit("(", 1)[-1].removesuffix(")")
    if not (len(team) == 10 and team.isascii() and team.isalnum()):
        raise ValueError("Expected a Developer ID identity with an Apple team ID")
    if archive.is_symlink() or archive.exists() or archive.suffix != ".zip":
        raise ValueError("Select a new ZIP output")
    if shutil.disk_usage(archive.parent).free < 20 * 1024**3:
        raise ValueError("Less than 20 GiB available")
    manifest = desktop.with_suffix(".app.component-manifest.json")
    verifier = desktop / "Contents/Resources/verify-components.sh"
    run("bash", verifier, "verify", "--root", desktop, "--manifest", manifest)
    verify_app(desktop, "com.dicklesworthstone.frankenterm", team)
    run("lipo", helper, "-verify_arch", "arm64")
    work = Path(tempfile.mkdtemp(prefix="ft-buyer-", dir=archive.parent))
    export = build_export(source, work, helper)
    run(sys.executable, source / "scripts/sign-macos-bundle.py", export, identity, "--notarize")
    staged = work / archive.name
    # The full desktop app is distributed separately. Multiple top-level items
    # in an .app.zip cause Finder to create a non-launchable .app wrapper.
    write_export_archive(export, staged)
    unpacked = work / "unpacked"
    run("ditto", "-x", "-k", staged, unpacked)
    verify_app(unpacked / export.name, "org.frankenterm.offline-export", team)
    # Expose the completed archive atomically without replacing previous bytes.
    os.link(staged, archive)
    print(f"Verified buyer ZIP: {archive}")


if __name__ == "__main__":
    main()
