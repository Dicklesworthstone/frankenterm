#!/bin/bash
# DSR native-host build step. Produces a local App Store package; never uploads.
# Run only in DSR's isolated source export after its source/admission checks.
set -euo pipefail

fail() { echo "error: $*" >&2; exit 1; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ "${DSR_RELEASE_GIT_SHA:-}" =~ ^[0-9a-f]{40}$ ]] || fail "DSR release source identity required"
[[ "$DSR_RELEASE_GIT_SHA" != 0000000000000000000000000000000000000000 ]] || fail "invalid source identity"
[[ -n "${DSR_RELEASE_GIT_REF:-}" ]] || fail "DSR release ref required"
[[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" = /* ]] || fail "isolated absolute CARGO_TARGET_DIR required"
[[ "${FT_IOS_VERSION:-}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "FT_IOS_VERSION must be explicit"
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
python3 - "$CARGO_TARGET_DIR" <<'PY'
import pathlib, shutil, sys
p = pathlib.Path(sys.argv[1])
while not p.exists():
    p = p.parent
if shutil.disk_usage(p).free < 20 * 1024**3:
    raise SystemExit("error: fewer than 20 GiB free")
PY
}
check_space
mkdir -p "$OUT"
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
    CURRENT_PROJECT_VERSION="$FT_IOS_BUILD" archive
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
(cd "$OUT" && shasum -a 256 export/FrankenTerm.ipa native-library-inputs.json > SHA256SUMS)
echo "App Store package retained at $OUT; runtime qualification and submission remain separate."
