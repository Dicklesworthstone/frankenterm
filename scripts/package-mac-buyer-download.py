"""Build the matching Export app and pack both notarized apps for iOS buyers."""

import argparse
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile


def run(*args):
    subprocess.run(args, check=True)


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
        ["plutil", "-convert", "xml1", "-o", "-", original / "project.pbxproj"]))
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
        ["codesign", "--display", "--entitlements", "-", "--xml", app]))
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
    payload = work / "download"
    payload.mkdir()
    for app in (desktop, export):
        run("ditto", app, payload / app.name)
    shutil.copy2(manifest, payload / manifest.name)
    staged = work / archive.name
    run("ditto", "-c", "-k", "--sequesterRsrc", payload, staged)
    unpacked = work / "unpacked"
    run("ditto", "-x", "-k", staged, unpacked)
    verify_app(unpacked / desktop.name, "com.dicklesworthstone.frankenterm", team)
    verify_app(unpacked / export.name, "org.frankenterm.offline-export", team)
    run("bash", unpacked / desktop.name / "Contents/Resources/verify-components.sh",
        "verify", "--root", unpacked / desktop.name, "--manifest", unpacked / manifest.name)
    # Expose the completed archive atomically without replacing previous bytes.
    os.link(staged, archive)
    print(f"Verified buyer ZIP: {archive}")


if __name__ == "__main__":
    main()
