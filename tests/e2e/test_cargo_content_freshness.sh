#!/usr/bin/env bash
# ft-bpeje: run only through strict RCH; retain the fixture and both warm caches.
set -euo pipefail
if [[ "$(uname -s)" != Linux || "${RCH_CARGO_WRAPPER_BYPASS:-}" != 1 || -z "${SSH_CONNECTION:-}" ]]; then
  echo 'Refusing local Cargo: requires Linux SSH worker and RCH-provided loop-break environment.' >&2
  exit 2
fi
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 - "$repo_root" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import tomllib

repo = Path(sys.argv[1])
config_bytes = (repo / '.cargo/config.toml').read_bytes()
config = tomllib.loads(config_bytes.decode())
assert config['build']['fingerprint'] == 'content', 'repository must select content fingerprints'
assert config['unstable']['checksum-freshness'] is True
toolchain = tomllib.loads((repo / 'rust-toolchain.toml').read_text())['toolchain']['channel']
assert toolchain.startswith('nightly-'), 'requires repository-pinned nightly'
scratch = Path(tempfile.mkdtemp(prefix='ft-bpeje-content-freshness-'))
print(f'Retained fixture: {scratch}', flush=True)
env = {k: v for k, v in os.environ.items()
       if not k.startswith(('CARGO_', 'RUST'))}
cargo = subprocess.check_output(['rustup', 'which', '--toolchain', toolchain, 'cargo'],
                                env=env, text=True, timeout=30).strip()
rustc = subprocess.check_output(['rustup', 'which', '--toolchain', toolchain, 'rustc'],
                                env=env, text=True, timeout=30).strip()
env['RUSTC'] = rustc
env['CARGO_HOME'] = str(scratch / 'cargo-home')
for executable in (cargo, rustc):
    print(subprocess.check_output([executable, '--version'], env=env,
                                  text=True, timeout=30).strip(), flush=True)
v1 = b'fn main() { println!("41"); }\n'
v2 = b'fn main() { println!("42"); }\n'
assert len(v1) == len(v2) and v1 != v2
results = {}
for lane in ('mtime', 'content'):
    root = scratch / lane
    (root / 'src').mkdir(parents=True)
    (root / '.cargo').mkdir()
    (root / '.cargo/config.toml').write_bytes(config_bytes)
    (root / 'Cargo.toml').write_text(
        '[package]\nname = "freshness_probe"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n')
    source = root / 'src/main.rs'
    source.write_bytes(v1)
    original_ns = time.time_ns() - 60_000_000_000
    os.utime(source, ns=(original_ns, original_ns))
    command = [cargo, 'build', '--offline', '--target-dir', str(root / 'target')]
    # Override only the selector for the negative control; keep checksum-freshness
    # enabled in both lanes, matching the previously insufficient configuration.
    if lane == 'mtime':
        command += ['--config', 'build.fingerprint="mtime"']
    outputs = []
    for attempt in range(3):
        if attempt == 1:
            source.write_bytes(v2)
            older_ns = original_ns - 60_000_000_000
            os.utime(source, ns=(older_ns, older_ns))
            assert source.stat().st_mtime_ns < original_ns
            assert source.stat().st_size == len(v1)
        build = subprocess.run(command, cwd=root, env=env, capture_output=True,
                               text=True, timeout=120)
        (root / f'build-{attempt}.stdout').write_text(build.stdout)
        (root / f'build-{attempt}.stderr').write_text(build.stderr)
        assert build.returncode == 0, build.stderr
        output = subprocess.check_output([str(root / 'target/debug/freshness_probe')],
                                         env=env, text=True, timeout=10)
        outputs.append(output.strip())
        (root / f'run-{attempt}.stdout').write_text(output)
    expected = ['41', '41', '41'] if lane == 'mtime' else ['41', '42', '42']
    assert outputs == expected, f'{lane}: expected {expected}, observed {outputs}'
    results[lane] = outputs
    print(f'{lane}: {outputs}', flush=True)
receipt = {'toolchain': toolchain, 'config_sha256': hashlib.sha256(config_bytes).hexdigest(),
           'outputs': results, 'source_identity_authority': 'retained outer strict RCH receipt'}
(scratch / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
print('CARGO_CONTENT_FRESHNESS_PROOF_SUCCESS ' + json.dumps(receipt, sort_keys=True))
PY
