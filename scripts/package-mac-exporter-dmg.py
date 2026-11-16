"""Create a drag-to-Applications disk image from the signed, notarized exporter.

The legacy ZIP remains available for installed iOS versions. This additional
installer contains the small exporter only, never the full desktop terminal.
"""

import argparse
from pathlib import Path
import plistlib
import subprocess
import tempfile


def run(*args):
    subprocess.run(args, check=True, timeout=300, shell=False)


def create_image(app, output, team):
    app = app.resolve(strict=True)
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("Refusing to replace an existing disk image")
    if output.suffix != ".dmg" or app.name != "FrankenTermExport.app":
        raise ValueError("Expected FrankenTermExport.app and a new .dmg path")
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != "org.frankenterm.offline-export":
        raise ValueError("Not the standalone exporter")
    if (info.get("CFBundleExecutable") != "FrankenTermExport" or
            info.get("CFBundlePackageType") != "APPL"):
        raise ValueError("Incorrect exporter executable metadata")
    for name in ("FrankenTermExport", "frankenterm-machines"):
        binary = app / "Contents/MacOS" / name
        if not binary.is_file() or binary.is_symlink() or not binary.stat().st_mode & 0o100:
            raise ValueError(f"Missing executable: {name}")
    if info.get("CFBundleIconFile") not in ("Exporter", "Exporter.icns"):
        raise ValueError("Exporter icon metadata is missing")
    if not (app / "Contents/Resources/Exporter.icns").is_file():
        raise ValueError("Exporter icon resource is missing")
    if len(team) != 10 or not team.isascii() or not team.isalnum():
        raise ValueError("Expected a ten-character Apple team identifier")
    run("codesign", "--verify", "--deep", "--strict", "-R",
        f'=anchor apple generic and certificate leaf[subject.OU] = "{team}"', str(app))
    run("xcrun", "stapler", "validate", str(app))
    run("spctl", "--assess", "--type", "execute", str(app))
    # Retain staging for audit/recovery. Do not mutate the signed app bundle.
    stage = Path(tempfile.mkdtemp(prefix="exporter-dmg-", dir=output.parent))
    run("ditto", str(app), str(stage / app.name))
    (stage / "Applications").symlink_to("/Applications", target_is_directory=True)
    (stage / "Start Here.txt").write_text(
        "FrankenTerm Exporter\n\n"
        "1. Drag FrankenTermExport into Applications.\n"
        "2. Open FrankenTermExport from Applications, then eject this disk.\n"
        "3. Choose Import SSH Config or Open Workspace to select your machines.\n"
        "4. Follow the onscreen steps to send them securely to FrankenTerm on your iPhone.\n\n"
        "Included with FrankenTerm for iPhone and iPad. No separate purchase or installer required.\n"
        "Requires an Apple Silicon Mac with macOS 14 or later.\n"
        "This is the small transfer utility; it does not install the desktop terminal.\n",
        encoding="utf-8")
    run("hdiutil", "create", "-srcfolder", str(stage), "-volname",
        "FrankenTerm Exporter", "-format", "UDZO", str(output))
    run("hdiutil", "verify", str(output))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--team", required=True)
    args = parser.parse_args()
    create_image(args.app, args.output, args.team)
