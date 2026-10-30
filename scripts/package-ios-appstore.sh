#!/bin/bash
# DSR native-host build step. Produces a local App Store package; never uploads.
# Run only in DSR's isolated source export after its source/admission checks.
set -euo pipefail

fail() { echo "error: $*" >&2; exit 1; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "${DSR_RELEASE_GIT_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || fail "DSR release source identity required"
[[ "$DSR_RELEASE_GIT_SHA" != 0000000000000000000000000000000000000000 ]] || fail "invalid source identity"
[[ -n "${DSR_RELEASE_GIT_REF:-}" ]] || fail "DSR release ref required"
python3 - <<'PY'
import os, urllib.parse, urllib.request
value = os.environ.get('FT_INCLUDED_MAC_DOWNLOAD_URL', '')
url = urllib.parse.urlsplit(value)
parts = url.path.strip('/').split('/')
if (url.scheme != 'https' or url.netloc != 'github.com' or url.query or url.fragment
        or len(parts) != 6 or parts[:4] != ['Dicklesworthstone', 'frankenterm', 'releases', 'download']
        or parts[4] in ('', '.', '..', 'latest') or not parts[5].endswith(('.dmg', '.zip'))):
    raise SystemExit('error: FT_INCLUDED_MAC_DOWNLOAD_URL must identify a pinned Mac release asset')
request = urllib.request.Request(value, method='HEAD', headers={
    'User-Agent': 'frankenterm-agent (+https://github.com/Dicklesworthstone/frankenterm)'})
with urllib.request.urlopen(request, timeout=30) as response:
    if response.status != 200:
        raise SystemExit('error: included Mac download is unavailable')
PY
[[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" = /* ]] || fail "isolated absolute CARGO_TARGET_DIR required"
[[ "${FT_IOS_VERSION:-}" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || fail "FT_IOS_VERSION must be an explicit two- or three-component version"
[[ "${FT_IOS_BUILD:-}" =~ ^[1-9][0-9]*$ ]] || fail "FT_IOS_BUILD must be explicit"
[[ "${FT_APPLE_TEAM_ID:-}" =~ ^[A-Z0-9]{10}$ ]] || fail "FT_APPLE_TEAM_ID required"
# A live checkout may contain concurrent edits or existing development artifacts.
[[ ! -e "$ROOT/.git" ]] || fail "use DSR's isolated source export, not a live checkout"
for framework in FrankenTermTerminal/Artifacts/CFrankenTermTerminal.xcframework FrankenTermSSH/Artifacts/CLibssh2.xcframework; do
    [[ ! -e "$ROOT/apps/apple/$framework" ]] || fail "source export contains stale native artifacts: $framework"
done
OUT="$CARGO_TARGET_DIR/appstore-ios-$FT_IOS_VERSION-$FT_IOS_BUILD"
[[ ! -e "$OUT" ]] || fail "retain existing output; select a fresh DSR build destination"
check_space() {
python3 - "$CARGO_TARGET_DIR" "$ROOT" <<'PY'
import pathlib, shutil, sys
p = pathlib.Path(sys.argv[1])
if p.resolve().is_relative_to(pathlib.Path(sys.argv[2]).resolve()):
    raise SystemExit("error: build output must be outside the source export")
while not p.exists():
    p = p.parent
if shutil.disk_usage(p).free < 20 * 1024**3:
    raise SystemExit("error: fewer than 20 GiB free")
PY
}
check_space
mkdir -p "$OUT"
# SwiftPM requires its binary targets inside the package. Stage a byte-checked
# build tree outside DSR's immutable export rather than modifying that export.
stage_source() {
python3 - "$1" "$ROOT" "$OUT/source" "$OUT/staged-source.json" <<'PY'
import hashlib, json, os, pathlib, shutil, sys
mode, source, stage, receipt = sys.argv[1:]
source, stage, receipt = map(pathlib.Path, (source, stage, receipt))
def inventory(root):
    result = {}
    for base, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            p = pathlib.Path(base) / name
            key = p.relative_to(root).as_posix()
            if p.is_symlink():
                result[key] = ['link', os.readlink(p)]
            elif p.is_dir():
                result[key] = ['directory']
            elif p.is_file():
                with p.open('rb') as f:
                    digest = hashlib.sha256()
                    for block in iter(lambda: f.read(1024 * 1024), b''):
                        digest.update(block)
                    result[key] = ['file', digest.hexdigest(), p.stat().st_mode & 0o777]
            else:
                raise SystemExit(f'error: unsupported source entry: {key}')
    return result
if mode == 'prepare':
    if stage.resolve().is_relative_to(source.resolve()):
        raise SystemExit('error: mutable build staging must be outside source export')
    expected = inventory(source)
    shutil.copytree(source, stage, symlinks=True)
    if inventory(stage) != expected:
        raise SystemExit('error: staging copy differs from source export')
    with receipt.open('x') as f:
        json.dump(expected, f, sort_keys=True)
else:
    expected = json.loads(receipt.read_text())
    actual = inventory(stage)
    if any(actual.get(k) != v for k, v in expected.items()):
        raise SystemExit('error: staged source inputs changed during build')
    generated = (
        'apps/apple/FrankenTermTerminal/Artifacts',
        'apps/apple/FrankenTermSSH/Artifacts',
    )
    unexpected = [k for k in actual.keys() - expected.keys()
                  if not any(k == d or k.startswith(d + '/') for d in generated)]
    if unexpected:
        raise SystemExit(f'error: unexpected staged build inputs: {sorted(unexpected)[:10]}')
PY
}
stage_source prepare
ROOT="$OUT/source"
export FT_APPLE_BUILD_JOBS=2
export FT_APPLE_TARGET_DIR="$CARGO_TARGET_DIR/apple-terminal"
export FT_APPLE_SSH_BUILD_DIR="$CARGO_TARGET_DIR/apple-ssh"
# Rebuild both native dependencies from this export, never reuse a checked-out
# XCFramework. DSR retains source identity and this complete build transcript.
bash "$ROOT/scripts/build-apple-terminal-xcframework.sh"
check_space
bash "$ROOT/scripts/build-apple-ssh-xcframework.sh"
python3 - "$ROOT" "$OUT" <<'PY'
import hashlib, json, os, pathlib, sys
root, out = map(pathlib.Path, sys.argv[1:])
inputs = {}
for package in ('FrankenTermTerminal', 'FrankenTermSSH'):
    for p in sorted((root / 'apps/apple' / package / 'Artifacts').rglob('*')):
        if p.is_file():
            inputs[str(p.relative_to(root))] = hashlib.sha256(p.read_bytes()).hexdigest()
(out / 'native-library-inputs.json').write_text(json.dumps({
    'source_revision': os.environ['DSR_RELEASE_GIT_SHA'],
    'source_ref': os.environ['DSR_RELEASE_GIT_REF'], 'files': inputs}, indent=2) + '\n')
PY
check_space
xcodebuild -project "$ROOT/apps/apple/FrankenTerm.xcodeproj" -scheme FrankenTerm \
    -configuration Release -destination 'generic/platform=iOS' -jobs 2 \
    -onlyUsePackageVersionsFromResolvedFile \
    -derivedDataPath "$OUT/DerivedData" -clonedSourcePackagesDirPath "$OUT/Packages" \
    -archivePath "$OUT/FrankenTerm.xcarchive" \
    DEVELOPMENT_TEAM="$FT_APPLE_TEAM_ID" MARKETING_VERSION="$FT_IOS_VERSION" \
    CURRENT_PROJECT_VERSION="$FT_IOS_BUILD" \
    FT_INCLUDED_MAC_DOWNLOAD_URL="$FT_INCLUDED_MAC_DOWNLOAD_URL" archive
python3 - "$OUT" <<'PY'
import os, pathlib, plistlib, sys
out = pathlib.Path(sys.argv[1])
with (out / 'ExportOptions.plist').open('xb') as f:
    plistlib.dump({'method': 'app-store-connect', 'destination': 'export',
                  'teamID': os.environ['FT_APPLE_TEAM_ID'], 'signingStyle': 'automatic',
                  'manageAppVersionAndBuildNumber': False, 'uploadSymbols': False}, f)
app = out / 'FrankenTerm.xcarchive/Products/Applications/FrankenTerm.app'
info = plistlib.loads((app / 'Info.plist').read_bytes())
expected = {'CFBundleIdentifier': 'org.frankenterm.mobile',
            'FTIncludedMacDownloadURL': os.environ['FT_INCLUDED_MAC_DOWNLOAD_URL'],
            'CFBundleShortVersionString': os.environ['FT_IOS_VERSION'],
            'CFBundleVersion': os.environ['FT_IOS_BUILD'],
            'CFBundleSupportedPlatforms': ['iPhoneOS']}
for key, value in expected.items():
    if info.get(key) != value:
        raise SystemExit(f'error: unexpected archive {key}')
if set(info.get('UIDeviceFamily', [])) != {1, 2}:
    raise SystemExit('error: archive must support iPhone and iPad')
privacy = plistlib.loads((app / 'PrivacyInfo.xcprivacy').read_bytes())
if privacy.get('NSPrivacyTracking') is not False:
    raise SystemExit('error: review the changed tracking declaration before packaging')
PY
codesign --verify --deep --strict "$OUT/FrankenTerm.xcarchive/Products/Applications/FrankenTerm.app"
check_space
xcodebuild -exportArchive -archivePath "$OUT/FrankenTerm.xcarchive" \
    -exportOptionsPlist "$OUT/ExportOptions.plist" -exportPath "$OUT/export"
[[ -f "$OUT/export/FrankenTerm.ipa" ]] || fail "export did not produce FrankenTerm.ipa"
stage_source verify
(cd "$OUT" && shasum -a 256 export/FrankenTerm.ipa native-library-inputs.json > SHA256SUMS)
echo "App Store package retained at $OUT; runtime qualification and submission remain separate."
