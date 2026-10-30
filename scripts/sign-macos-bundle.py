#!/usr/bin/env python3
"""Sign a freshly assembled Mac bundle inside-out before manifest generation.

Optional notarization uses the existing ASC authentication and requires accepted,
stapled, Gatekeeper-validated output. This does not publish, add sandbox
restrictions, or grant new entitlements. Existing nested-code entitlements are
preserved; full-feature hardened-runtime testing remains required before distribution.
"""
import os
import json
import pathlib
import subprocess
import sys


def sign_bundle(root: pathlib.Path, identity: str) -> None:
    if not identity.startswith('Developer ID Application: '):
        raise ValueError('an explicit Developer ID Application identity is required')
    if root.is_symlink() or not root.is_dir() or root.suffix != '.app':
        raise ValueError('expected a plain application bundle directory')
    if not (root / 'Contents/Info.plist').is_file():
        raise ValueError('application Info.plist is missing')
    magic = {bytes.fromhex(value) for value in (
        'feedface', 'cefaedfe', 'feedfacf', 'cffaedfe',
        'cafebabe', 'bebafeca', 'cafebabf', 'bfbafeca',
    )}
    binaries = []
    bundles = []
    for base, directories, files in os.walk(root, followlinks=False):
        for name in directories:
            path = pathlib.Path(base) / name
            if not path.is_symlink() and path.suffix in {'.app', '.framework', '.xpc', '.appex'}:
                bundles.append(path)
        for name in files:
            path = pathlib.Path(base) / name
            if path.is_symlink() or not path.is_file():
                continue
            with path.open('rb') as handle:
                if handle.read(4) not in magic:
                    continue
            # Java class files share a fat-Mach-O magic; inspect actual format.
            kind = subprocess.run(['file', '-b', str(path)], check=True,
                                  capture_output=True, text=True, timeout=30).stdout
            if 'Mach-O' in kind:
                binaries.append(path)
    if not binaries:
        raise ValueError('bundle contains no Mach-O code')
    ordered = sorted(binaries, key=str)
    ordered += sorted(bundles, key=lambda path: (-len(path.parts), str(path)))
    ordered.append(root)
    for path in ordered:
        subprocess.run([
            'codesign', '--force', '--sign', identity, '--timestamp',
            '--options', 'runtime', '--preserve-metadata=entitlements', str(path),
        ], check=True, timeout=120)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(root)],
                   check=True, timeout=120)
    print(f'Developer ID signed {len(binaries)} Mach-O files and {len(bundles) + 1} bundles; notarization pending.')


def notarization_paths(root: pathlib.Path):
    archive = root.with_suffix('.notarization.zip')
    receipt = root.with_suffix('.notarization.json')
    if archive.exists() or receipt.exists():
        raise ValueError('retain prior notarization evidence; use a fresh output directory')
    return archive, receipt


def notarize_bundle(root: pathlib.Path) -> None:
    archive, receipt = notarization_paths(root)
    subprocess.run(['ditto', '-c', '-k', '--keepParent', str(root), str(archive)],
                   check=True, timeout=600)
    try:
        result = subprocess.run([
            'asc', 'notarization', 'submit', '--file', str(archive), '--wait',
            '--timeout', '1h', '--output', 'json',
        ], capture_output=True, text=True, timeout=3700)
    except subprocess.TimeoutExpired as error:
        def text(value):
            return value.decode('utf-8', errors='replace') if isinstance(value, bytes) else (value or '')
        with receipt.open('x') as output:
            json.dump({'timed_out': True, 'stdout': text(error.stdout), 'stderr': text(error.stderr)}, output)
        raise RuntimeError(f'notarization outcome unknown; reconcile {receipt} before retrying') from error
    # Retain Apple's response even on rejection; never turn a retry into an
    # unnoticed duplicate submission or treat a transport timeout as acceptance.
    with receipt.open('x') as output:
        json.dump({'returncode': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}, output)
    if result.returncode:
        raise RuntimeError(f'notarization did not complete successfully; inspect {receipt}')
    try:
        response = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f'could not parse Apple notarization response; inspect {receipt}') from error
    attributes = response.get('data', {}).get('attributes', {})
    if attributes.get('status') != 'Accepted':
        raise RuntimeError(f'notarization acceptance is unproven; inspect {receipt}')
    subprocess.run(['xcrun', 'stapler', 'staple', str(root)], check=True, timeout=120)
    subprocess.run(['xcrun', 'stapler', 'validate', str(root)], check=True, timeout=120)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(root)],
                   check=True, timeout=120)
    subprocess.run(['spctl', '--assess', '--type', 'execute', str(root)],
                   check=True, timeout=120)
    print('Apple notarization accepted; stapled ticket and Gatekeeper assessment passed.')


if __name__ == '__main__':
    if len(sys.argv) not in (3, 4) or (len(sys.argv) == 4 and sys.argv[3] != '--notarize'):
        raise SystemExit('usage: sign-macos-bundle.py FRESH_APP_BUNDLE DEVELOPER_ID_IDENTITY [--notarize]')
    root = pathlib.Path(sys.argv[1])
    # Refuse a retry before codesign can alter previously qualified bytes.
    if len(sys.argv) == 4:
        notarization_paths(root)
    sign_bundle(root, sys.argv[2])
    if len(sys.argv) == 4:
        notarize_bundle(root)
