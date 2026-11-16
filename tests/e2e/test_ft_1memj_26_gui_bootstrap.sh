#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Exercise the exact embedded packager with controlled Mach-O tool responses.
# These are portable graph/guard tests, not native linker or launch evidence.
test_native_dependency_closure() {
  python3 - "${ROOT_DIR}/scripts/create-macos-bundle.sh" <<'PY'
import json
from pathlib import Path
import plistlib
import shutil
import sys
import tempfile
import unittest

source = Path(sys.argv[1]).read_text()
body = source.split("<<'PY_NATIVE_DYLIB_CLOSURE'\n", 1)[1].split("\nPY_NATIVE_DYLIB_CLOSURE\n", 1)[0]
module = {"__name__": "native_dependency_packager_test"}
exec(compile(body, str(sys.argv[1]) + ":native_dependencies", "exec"), module)


class NativeDependencyClosure(unittest.TestCase):
    def setUp(self):
        # Retain failed as well as passing fixtures for diagnosis; no deletion.
        self.root = Path(tempfile.mkdtemp(prefix="ft-native-dependency-test-")).resolve()
        self.binary_dir = self.root / "build"
        self.binary_dir.mkdir()
        self.bundle = self.root / "FrankenTerm.app"
        (self.bundle / "Contents/MacOS").mkdir(parents=True)
        (self.bundle / "Contents/Resources").mkdir()
        (self.bundle / "Contents/Info.plist").write_bytes(plistlib.dumps({'LSMinimumSystemVersion': '11.0'}))
        self.images = {}
        self.changed = []
        self.ignore_rewrites = False
        for name in ("frankenterm-gui", "frankenterm-mux-server", "frankenterm-pty-guardian", "ft"):
            path = self.binary_dir / name
            path.write_bytes(("image:" + name).encode())
            shutil.copyfile(path, self.bundle / "Contents/MacOS" / name)
            self.images[path] = {"loads": ["/usr/lib/libSystem.B.dylib"], "ids": [], "rpaths": [], "arch": "arm64"}
        self.gui = self.binary_dir / "frankenterm-gui"
        module["command"] = self.command

    def dylib(self, package, name, *, notices=True):
        root = self.root / "Cellar" / package / "1.0"
        (root / "lib").mkdir(parents=True, exist_ok=True)
        (root / "INSTALL_RECEIPT.json").write_text('{}')
        if notices:
            (root / "COPYING").write_text('fixture notice')
        path = root / "lib" / name
        path.write_bytes(("dylib:" + package + name).encode())
        self.images[path] = {"loads": ["/usr/lib/libSystem.B.dylib"], "ids": [str(path)], "rpaths": [], "arch": "arm64"}
        return path

    def command(self, tool, *args):
        path = Path(args[-1])
        if path not in self.images:
            matches = [value for key, value in self.images.items() if key.name == path.name and not key.is_relative_to(self.bundle)]
            self.assertEqual(len(matches), 1)
            self.images[path] = json.loads(json.dumps(matches[0]))
        image = self.images[path]
        if tool == "lipo":
            return image['arch'] + '\n'
        if tool == "otool":
            records = [('LC_ID_DYLIB', 'name', value) for value in image['ids']]
            records += [('LC_LOAD_DYLIB', 'name', value) for value in image['loads']]
            records += [('LC_RPATH', 'path', value) for value in image['rpaths']]
            deployment = image.get('deployment', 'LC_BUILD_VERSION')
            minimum = image.get('minimum', '11.0')
            platform = image.get('platform', '1')
            build = (f'Load command {len(records)}\n cmd {deployment}\n platform {platform}\n minos {minimum}\n'
                     if deployment == 'LC_BUILD_VERSION' else
                     f'Load command {len(records)}\n cmd {deployment}\n version {minimum}\n')
            return str(path) + ':\n' + ''.join(
                f'Load command {i}\n cmd {kind}\n {field} {value} (offset 24)\n'
                for i, (kind, field, value) in enumerate(records)) + build
        self.assertEqual(tool, 'install_name_tool')
        self.assertTrue(path.is_relative_to(self.bundle))
        self.changed.append(path)
        if self.ignore_rewrites:
            return ''
        index = 0
        while index < len(args) - 1:
            option, old = args[index:index + 2]
            if option == '-change':
                new = args[index + 2]
                image['loads'] = [new if name == old else name for name in image['loads']]
                index += 3
            elif option == '-id':
                image['ids'] = [old]
                index += 2
            elif option == '-delete_rpath':
                image['rpaths'].remove(old)
                index += 2
            else:
                self.fail(option)
        with path.open('ab') as handle:
            handle.write(b':relocated')
        return ''

    def package(self):
        module['bundle_native_dependencies'](self.bundle, self.binary_dir, 'arm64')

    def test_transitive_alias_cycle_relocated_with_notices_and_unchanged_sources(self):
        cairo = self.dylib('cairo', 'libcairo.2.dylib')
        png = self.dylib('libpng', 'libpng.16.dylib')
        alias = self.root / 'opt-cairo'
        alias.symlink_to(cairo.parent, target_is_directory=True)
        self.images[self.gui]['loads'].append(str(alias / cairo.name))
        self.images[self.gui]['rpaths'] = ['/opt/homebrew/lib']
        self.images[cairo]['loads'].append(str(png))
        self.images[png]['loads'].append(str(cairo))
        before = {path: path.read_bytes() for path in self.images}
        self.package()
        frameworks = self.bundle / 'Contents/Frameworks'
        self.assertEqual(sorted(path.name for path in frameworks.iterdir()), sorted([cairo.name, png.name]))
        for path, expected in before.items():
            self.assertEqual(path.read_bytes(), expected)
        gui = self.images[self.bundle / 'Contents/MacOS/frankenterm-gui']
        self.assertEqual(gui['loads'][-1], '@loader_path/../Frameworks/libcairo.2.dylib')
        self.assertEqual(gui['rpaths'], [])
        self.assertEqual(self.images[frameworks / png.name]['loads'][-1], '@loader_path/libcairo.2.dylib')
        metadata = self.bundle / 'Contents/Resources/native-dependencies'
        self.assertEqual((metadata / cairo.name / 'COPYING').read_text(), 'fixture notice')
        self.assertEqual(len(json.loads((metadata / 'closure.json').read_text())['images']), 6)

    def test_case_insensitive_collision_refused_before_rewrites(self):
        one = self.dylib('one', 'libA.dylib')
        two = self.dylib('two', 'liba.dylib')
        self.images[self.gui]['loads'] += [str(one), str(two)]
        with self.assertRaisesRegex(ValueError, 'basename collision'):
            self.package()
        self.assertEqual(self.changed, [])

    def test_unresolved_rpath_refused(self):
        self.images[self.gui]['loads'].append('@rpath/libmissing.dylib')
        with self.assertRaisesRegex(ValueError, 'unresolved native dependency'):
            self.package()

    def test_missing_transitive_file_refused(self):
        cairo = self.dylib('cairo', 'libcairo.dylib')
        self.images[self.gui]['loads'].append(str(cairo))
        self.images[cairo]['loads'].append(str(self.root / 'absent.dylib'))
        with self.assertRaises(FileNotFoundError):
            self.package()
        self.assertEqual(self.changed, [])

    def test_wrong_architecture_refused(self):
        self.images[self.gui]['arch'] = 'x86_64'
        with self.assertRaisesRegex(ValueError, 'wrong architecture'):
            self.package()

    def test_missing_license_refused(self):
        library = self.dylib('no-notices', 'libmissing.dylib', notices=False)
        self.images[self.gui]['loads'].append(str(library))
        with self.assertRaisesRegex(ValueError, 'license notices unavailable'):
            self.package()

    def test_tool_success_without_relocation_is_not_success(self):
        library = self.dylib('cairo', 'libcairo.dylib')
        self.images[self.gui]['loads'].append(str(library))
        self.ignore_rewrites = True
        with self.assertRaisesRegex(ValueError, 'differ from the planned closure'):
            self.package()

    def test_successful_tool_must_not_drop_dependencies(self):
        library = self.dylib('cairo', 'libcairo.dylib')
        self.images[self.gui]['loads'].append(str(library))
        normal_command = self.command

        def dropping_command(tool, *args):
            result = normal_command(tool, *args)
            if tool == 'install_name_tool':
                self.images[Path(args[-1])]['loads'] = ['/usr/lib/libSystem.B.dylib']
            return result

        module['command'] = dropping_command
        with self.assertRaisesRegex(ValueError, 'differ from the planned closure'):
            self.package()

    def test_empty_successful_otool_output_is_refused(self):
        normal_command = self.command
        module['command'] = lambda tool, *args: '' if tool == 'otool' else normal_command(tool, *args)
        with self.assertRaisesRegex(ValueError, 'no readable load commands'):
            self.package()

    def test_preexisting_frameworks_not_merged(self):
        (self.bundle / 'Contents/Frameworks').mkdir()
        with self.assertRaises(FileExistsError):
            self.package()

    def test_transitive_dependency_sets_actual_bundle_deployment_floor(self):
        cairo = self.dylib('cairo', 'libcairo.dylib')
        png = self.dylib('libpng', 'libpng.dylib')
        self.images[self.gui]['loads'].append(str(cairo))
        self.images[cairo]['loads'].append(str(png))
        self.images[cairo]['minimum'] = '15.0'
        self.images[png]['minimum'] = '26.0'
        self.package()
        info = plistlib.loads((self.bundle / 'Contents/Info.plist').read_bytes())
        self.assertEqual(info['LSMinimumSystemVersion'], '26.0.0')
        receipt = json.loads((self.bundle / 'Contents/Resources/native-dependencies/closure.json').read_text())
        self.assertEqual(receipt['minimum_macos_version'], '26.0.0')
        self.assertEqual(receipt['bundle_minimum_macos_version'], '26.0.0')
        record = next(row for row in receipt['images'] if row['source'] == str(png))
        self.assertEqual(record['minimum_macos_version'], '26.0.0')

    def test_existing_higher_plist_floor_is_not_lowered(self):
        (self.bundle / 'Contents/Info.plist').write_bytes(plistlib.dumps({'LSMinimumSystemVersion': '26.2.1'}))
        self.package()
        info = plistlib.loads((self.bundle / 'Contents/Info.plist').read_bytes())
        self.assertEqual(info['LSMinimumSystemVersion'], '26.2.1')

    def test_legacy_macos_deployment_command_is_supported(self):
        self.images[self.gui].update(deployment='LC_VERSION_MIN_MACOSX', minimum='12.3')
        self.package()
        info = plistlib.loads((self.bundle / 'Contents/Info.plist').read_bytes())
        self.assertEqual(info['LSMinimumSystemVersion'], '12.3.0')

    def test_ios_image_is_rejected_even_with_correct_cpu(self):
        self.images[self.gui]['platform'] = '2'
        with self.assertRaisesRegex(ValueError, 'not built for macOS'):
            self.package()

    def test_missing_deployment_target_is_rejected(self):
        self.images[self.gui]['deployment'] = 'LC_UUID'
        with self.assertRaisesRegex(ValueError, 'one unambiguous macOS deployment target'):
            self.package()


unittest.main(argv=['native_dependency_closure'], verbosity=2)
PY
}

if [[ "${1:-}" == "--native-dependencies-only" ]]; then
  test_native_dependency_closure
  exit "$?"
fi

LOG_DIR="${ROOT_DIR}/tests/e2e/logs"
RUN_ID="$(date -u +"%Y%m%d_%H%M%S")"
ARTIFACT_DIR="${ROOT_DIR}/tests/e2e/artifacts/gui_bootstrap/${RUN_ID}"
SCENARIO_ID="ft_1memj_26_gui_bootstrap"
CORRELATION_ID="ft-1memj.26-${RUN_ID}"
LOG_FILE="${LOG_DIR}/${SCENARIO_ID}_${RUN_ID}.jsonl"
BUNDLE_TARGET="aarch64-apple-darwin"
BROWSER_RUNTIME_ROOT="${ARTIFACT_DIR}/browser-runtime-component"
BROWSER_RUNTIME_MANIFEST="${ARTIFACT_DIR}/browser-runtime-component.json"
BROWSER_BUNDLE_ARGS=()
WORKSPACE_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/Cargo.toml" | head -1)"
SOURCE_REVISION="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
TEST_ATOMIC_BUILD_ID="$(bash "${ROOT_DIR}/scripts/atomic-component-manifest.sh" derive-build-id \
  --source-revision "${SOURCE_REVISION}" \
  --version "${WORKSPACE_VERSION}" \
  --target "${BUNDLE_TARGET}" \
  --profile release-interactive \
  --feature-contract application-family-gui-ft-mux-server-pty-guardian-default-features-v1)"

mkdir -p "${LOG_DIR}" "${ARTIFACT_DIR}"

PASS=0
FAIL=0
TOTAL=0

# shellcheck source=tests/e2e/lib_rch_guards.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib_rch_guards.sh"
rch_init "${LOG_DIR}" "${RUN_ID}" "1memj_26_gui_bootstrap"

emit_log() {
  local outcome="$1"
  local scenario="$2"
  local decision_path="$3"
  local reason_code="$4"
  local error_code="$5"
  local artifact_path="$6"
  local input_summary="$7"
  local ts
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  jq -cn \
    --arg timestamp "${ts}" \
    --arg component "gui_bootstrap_contract.e2e" \
    --arg scenario_id "${SCENARIO_ID}:${scenario}" \
    --arg correlation_id "${CORRELATION_ID}" \
    --arg decision_path "${decision_path}" \
    --arg input_summary "${input_summary}" \
    --arg outcome "${outcome}" \
    --arg reason_code "${reason_code}" \
    --arg error_code "${error_code}" \
    --arg artifact_path "${artifact_path}" \
    '{
      timestamp: $timestamp,
      component: $component,
      scenario_id: $scenario_id,
      correlation_id: $correlation_id,
      decision_path: $decision_path,
      input_summary: $input_summary,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      artifact_path: $artifact_path
    }' >> "${LOG_FILE}"
}

record_result() {
  local name="$1"
  local ok="$2"
  local reason_code="${3:-completed}"
  local error_code="${4:-none}"
  local input_summary="${5:-}"

  TOTAL=$((TOTAL + 1))
  if [[ "${ok}" == "true" ]]; then
    PASS=$((PASS + 1))
    emit_log "passed" "${name}" "scenario_end" "${reason_code}" "none" "${LOG_FILE}" "${input_summary}"
    echo "  PASS: ${name}"
  else
    FAIL=$((FAIL + 1))
    emit_log "failed" "${name}" "scenario_end" "${reason_code}" "${error_code}" "${LOG_FILE}" "${input_summary}"
    echo "  FAIL: ${name}"
  fi
}

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "missing required command: ${cmd}" >&2
    exit 1
  fi
}

write_never_called_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/rch" <<EOF
#!/bin/bash
set -euo pipefail
printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 97
EOF
  chmod +x "${mock_bin}/rch"
}

write_probe_failure_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
cat > "${mock_bin}/rch" <<EOF
#!/bin/bash
set -euo pipefail
if [[ "\${1:-}" == "--no-self-healing" ]]; then
  shift
fi
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  printf '%s\n' '{"api_version":"1.0","data":{"results":[{"id":"mock-worker","host":"127.0.0.1","status":"connection_failed","error":"RCH-E100"}],"summary":{"healthy":0}}}'
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  printf 'unexpected exec: %s\n' "\$*" >> "${marker_file}"
  exit 0
fi

printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_success_build_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
cat > "${mock_bin}/rch" <<EOF
#!/bin/bash
set -euo pipefail
if [[ "\${1:-}" == "--no-self-healing" ]]; then
  shift
fi
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  printf '%s\n' '{"api_version":"1.0","data":{"results":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}],"summary":{"healthy":1}}}'
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  shift
  printf '%s\n' "\$*" > "${marker_file}"
  if [[ "\$*" == *"--clean-overlay"* && "\$*" == *"--source-content-receipt"* ]]; then
    echo "--clean-overlay conflicts with --source-content-receipt" >&2
    exit 2
  fi
  if [[ "\$*" == *"pkg-config --exists x11"* ]]; then
    exit 0
  fi
  if [[ "\$*" != *"cargo build"* ]]; then
    printf 'unexpected exec: %s\n' "\$*" >&2
    exit 64
  fi
  target_dir=""
  for arg in "\$@"; do
    if [[ "\${arg}" == CARGO_TARGET_DIR=* ]]; then
      target_dir="\${arg#CARGO_TARGET_DIR=}"
      break
    fi
  done
  if [[ -z "\${target_dir}" ]]; then
    echo "missing CARGO_TARGET_DIR" >&2
    exit 64
  fi
  mkdir -p "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive"
  printf '%s\n' \
    '#!/bin/bash' \
    'if [[ "${1:-}" == "--version" ]]; then' \
    '  echo "stub 0.0.0"' \
    '  exit 0' \
    'fi' \
    'if [[ "${1:-}" == "--help" ]]; then' \
    '  echo "stub help"' \
    '  exit 0' \
    'fi' \
    'exit 0' > "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui"
  printf '%s\n' '# FT_ATOMIC_COMPONENT_IDENTITY_V1:${TEST_ATOMIC_BUILD_ID}:frankenterm-gui:${BUNDLE_TARGET}:release-interactive:${WORKSPACE_VERSION};' >> "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui"
  printf '%s\n' \
    '#!/bin/bash' \
    'if [[ "${1:-}" == "--version" ]]; then' \
    '  echo "stub 0.0.0"' \
    '  exit 0' \
    'fi' \
    'if [[ "${1:-}" == "--help" ]]; then' \
    '  echo "stub help"' \
    '  exit 0' \
    'fi' \
    'exit 0' > "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/ft"
  printf '%s\n' '# FT_ATOMIC_COMPONENT_IDENTITY_V1:${TEST_ATOMIC_BUILD_ID}:ft:${BUNDLE_TARGET}:release-interactive:${WORKSPACE_VERSION};' >> "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/ft"
  printf '%s\n' \
    '#!/bin/bash' \
    'if [[ "${1:-}" == "--version" ]]; then' \
    '  echo "stub 0.0.0"' \
    '  exit 0' \
    'fi' \
    'if [[ "${1:-}" == "--help" ]]; then' \
    '  echo "stub help"' \
    '  exit 0' \
    'fi' \
    'exit 0' > "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server"
  printf '%s\n' '# FT_ATOMIC_COMPONENT_IDENTITY_V1:${TEST_ATOMIC_BUILD_ID}:frankenterm-mux-server:${BUNDLE_TARGET}:release-interactive:${WORKSPACE_VERSION};' >> "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server"
  printf '%s\n' \
    '#!/bin/bash' \
    'if [[ "${1:-}" == "--version" ]]; then' \
    '  echo "stub 0.0.0"' \
    '  exit 0' \
    'fi' \
    'if [[ "${1:-}" == "--help" ]]; then' \
    '  echo "stub help"' \
    '  exit 0' \
    'fi' \
    'exit 0' > "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  printf '%s\n' '# FT_ATOMIC_COMPONENT_IDENTITY_V1:${TEST_ATOMIC_BUILD_ID}:frankenterm-pty-guardian:${BUNDLE_TARGET}:release-interactive:${WORKSPACE_VERSION};' >> "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  chmod +x \
    "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui" \
    "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/ft" \
    "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server" \
    "\${PWD}/\${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  exit 0
fi

printf 'unexpected invocation: %s\n' "\$*" >> "${marker_file}"
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_missing_x11_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
cat > "${mock_bin}/rch" <<EOF
#!/bin/bash
set -euo pipefail
if [[ "\${1:-}" == "--no-self-healing" ]]; then
  shift
fi
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  printf '%s\n' '{"api_version":"1.0","data":{"results":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}],"summary":{"healthy":1}}}'
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  shift
  printf '%s\n' "\$*" >> "${marker_file}"
  if [[ "\$*" == *"cargo build"* ]]; then
    echo "Package x11 was not found in the pkg-config search path." >&2
    exit 42
  fi
  printf 'unexpected exec: %s\n' "\$*" >&2
  exit 64
fi

printf 'unexpected invocation: %s\n' "\$*" >&2
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_missing_gui_link_pkg_rch() {
  local mock_bin="$1"
  local marker_file="$2"
  local missing_pkg="$3"
  mkdir -p "${mock_bin}"
cat > "${mock_bin}/rch" <<EOF
#!/bin/bash
set -euo pipefail
if [[ "\${1:-}" == "--no-self-healing" ]]; then
  shift
fi
if [[ "\${1:-}" == "workers" && "\${2:-}" == "probe" ]]; then
  printf '%s\n' '{"api_version":"1.0","data":{"results":[{"id":"mock-worker","host":"127.0.0.1","status":"ok"}],"summary":{"healthy":1}}}'
  exit 0
fi

if [[ "\${1:-}" == "exec" ]]; then
  shift
  printf '%s\n' "\$*" >> "${marker_file}"
  if [[ "\$*" == *"cargo build"* ]]; then
    echo "Package ${missing_pkg} was not found in the pkg-config search path." >&2
    exit 43
  fi
  printf 'unexpected exec: %s\n' "\$*" >&2
  exit 64
fi

printf 'unexpected invocation: %s\n' "\$*" >&2
exit 64
EOF
  chmod +x "${mock_bin}/rch"
}

write_codesign_mock() {
  local mock_bin="$1"
  local marker_file="$2"
  mkdir -p "${mock_bin}"
  : > "${marker_file}"
  ln -sf /usr/bin/true "${mock_bin}/codesign"
  write_macho_tools_mock "${mock_bin}"
}

write_macho_tools_mock() {
  local mock_bin="$1"
  # Existing bundle-structure tests use shell stubs, not native artifacts.
  # Supply an explicit no-external-dependency Mach-O fixture for those tests.
  cat > "${mock_bin}/lipo" <<'EOF'
#!/bin/bash
printf '%s\n' arm64
EOF
  cat > "${mock_bin}/otool" <<'EOF'
#!/bin/bash
printf '%s\n' "$2:" 'Load command 0' ' cmd LC_LOAD_DYLIB' ' name /usr/lib/libSystem.B.dylib (offset 24)' 'Load command 1' ' cmd LC_BUILD_VERSION' ' platform 1' ' minos 11.0'
EOF
  chmod +x "${mock_bin}/lipo" "${mock_bin}/otool"
}

write_codesign_verification_failure_mock() {
  local mock_bin="$1"
  mkdir -p "${mock_bin}"
  cat > "${mock_bin}/codesign" <<'EOF'
#!/bin/bash
set -euo pipefail
if [[ "${1:-}" == "--verify" ]]; then
  exit 73
fi
exit 0
EOF
  chmod +x "${mock_bin}/codesign"
  write_macho_tools_mock "${mock_bin}"
}

write_stub_binary() {
  local path="$1"
  local component
  component="$(basename "${path}")"
  cat > "${path}" <<'EOF'
#!/bin/bash
if [[ "${1:-}" == "--version" ]]; then
  echo "stub 0.0.0"
  exit 0
fi
if [[ "${1:-}" == "--help" ]]; then
  echo "stub help"
  exit 0
fi
exit 0
EOF
  printf '%s\n' "# FT_ATOMIC_COMPONENT_IDENTITY_V1:${TEST_ATOMIC_BUILD_ID}:${component}:${BUNDLE_TARGET}:release-interactive:${WORKSPACE_VERSION};" >> "${path}"
  chmod +x "${path}"
}

prepare_browser_runtime_fixture() {
  local runtime="${BROWSER_RUNTIME_ROOT}/runtime"
  local source_revision
  local build_id

  mkdir -p \
    "${runtime}/bin" \
    "${runtime}/node_modules/playwright" \
    "${runtime}/browsers/chromium-fixture" \
    "${runtime}/licenses"
  printf '%s\n' '#!/bin/bash' 'exit 0' > "${runtime}/bin/node"
  printf '%s\n' 'module.exports = {};' > "${runtime}/node_modules/playwright/index.js"
  printf '%s\n' '#!/bin/bash' 'exit 0' > "${runtime}/browsers/chromium-fixture/chrome"
  printf '%s\n' 'node fixture license' > "${runtime}/licenses/node.txt"
  printf '%s\n' 'playwright fixture license' > "${runtime}/licenses/playwright.txt"
  printf '%s\n' 'chromium fixture notice' > "${runtime}/licenses/chromium.txt"
  printf '%s\n' '{"links":[],"schema_version":"ft.browser_runtime_symlinks.v1"}' \
    > "${runtime}/browser-symlinks.v1.json"
  chmod 0755 \
    "${runtime}/bin/node" \
    "${runtime}/browsers/chromium-fixture/chrome"

  source_revision="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
  build_id="$(bash "${ROOT_DIR}/scripts/atomic-component-manifest.sh" derive-build-id \
    --source-revision "${source_revision}" \
    --version fixture \
    --target "${BUNDLE_TARGET}" \
    --profile release-browser-runtime \
    --feature-contract test-fixture)"
  bash "${ROOT_DIR}/scripts/atomic-component-manifest.sh" generate \
    --root "${BROWSER_RUNTIME_ROOT}" \
    --source-root "${ROOT_DIR}" \
    --output "${BROWSER_RUNTIME_MANIFEST}" \
    --build-id "${build_id}" \
    --source-revision "${source_revision}" \
    --version fixture \
    --target "${BUNDLE_TARGET}" \
    --profile release-browser-runtime \
    --feature-contract test-fixture \
    --tree asset:browser-runtime:. \
    --contract browser.runtime.schema=playwright-chromium.v1 \
    --contract browser.runtime.target="${BUNDLE_TARGET}" \
    --contract browser.runtime.root=runtime \
    --contract browser.node.path=runtime/bin/node \
    --contract browser.node.version=fixture \
    --contract browser.playwright.module-path=runtime/node_modules/playwright/index.js \
    --contract browser.playwright.browsers-path=runtime/browsers \
    --contract browser.playwright.version=fixture \
    --contract browser.chromium.executable-path=runtime/browsers/chromium-fixture/chrome \
    --contract browser.chromium.revision=fixture \
    --contract browser.protocol.version=fixture \
    --contract browser.license.node-path=runtime/licenses/node.txt \
    --contract browser.license.playwright-path=runtime/licenses/playwright.txt \
    --contract browser.license.chromium-path=runtime/licenses/chromium.txt \
    --contract browser.symlink-manifest.path=runtime/browser-symlinks.v1.json \
    --contract browser.disk-budget.bytes=1048576 >/dev/null
  BROWSER_BUNDLE_ARGS=(
    --target "${BUNDLE_TARGET}"
    --browser-runtime-root "${BROWSER_RUNTIME_ROOT}"
    --browser-runtime-manifest "${BROWSER_RUNTIME_MANIFEST}"
  )
}

scenario_dry_run_skips_rch() {
  local scenario_dir="${ARTIFACT_DIR}/dry_run_skips_rch"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-invocations.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"

  mkdir -p "${scenario_dir}"
  write_never_called_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "dry_run_skips_rch" "dry_run" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh --dry-run"
  if env \
    RCH_BIN="${mock_bin}/rch" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --dry-run >"${stdout_file}" 2>"${stderr_file}"; then
    if [[ -f "${marker_file}" ]]; then
      record_result "dry_run_skips_rch" "false" "unexpected_rch_invocation" "RCH_CALLED" "dry-run touched mock rch"
      return
    fi
    if ! grep -Eq '\[DRY-RUN\].* exec -- env CARGO_TARGET_DIR=' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "missing_dry_run_build_line" "DRY_RUN_OUTPUT_MISSING" "dry-run output missing rch preview"
      return
    fi
    if grep -Fq "${ROOT_DIR}/Cargo.toml" "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "absolute_manifest_path" "ABSOLUTE_MANIFEST_PATH" "dry-run preview leaked local manifest path"
      return
    fi
    if grep -Fq "CARGO_TARGET_DIR=${ROOT_DIR}/" "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "absolute_target_dir" "ABSOLUTE_TARGET_DIR" "dry-run preview leaked local target path"
      return
    fi
    if ! grep -Fq -- '--manifest-path Cargo.toml' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "missing_relative_manifest_path" "MANIFEST_PATH_NOT_RELATIVE" "dry-run preview missing repo-relative manifest path"
      return
    fi
    if ! grep -q 'Summary: pass=3 fail=0 skip=4 total=7' "${stdout_file}"; then
      record_result "dry_run_skips_rch" "false" "unexpected_summary" "SUMMARY_MISMATCH" "unexpected dry-run summary"
      return
    fi
    record_result "dry_run_skips_rch" "true"
    return
  fi

  record_result "dry_run_skips_rch" "false" "dry_run_failed" "SCRIPT_EXIT_NONZERO" "script returned non-zero in dry-run"
}

scenario_e2e_probe_failure_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/e2e_probe_failure_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local probe_log="${scenario_dir}/rch-probe.json"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_probe_failure_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "e2e_probe_failure_refuses_exec" "probe_guard" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh probe failure"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    RCH_PROBE_LOG="${probe_log}" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --skip-bundle >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_success" "PROBE_GUARD_MISSING" "script unexpectedly succeeded"
    return
  fi
  if [[ -f "${marker_file}" ]]; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_exec" "RCH_EXEC_CALLED" "mock rch exec path was invoked"
    return
  fi
  if ! grep -q 'No reachable RCH workers detected; refusing local cargo fallback.' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "missing_guardrail_message" "FAIL_OPEN_MESSAGE_MISSING" "missing fail-closed message"
    return
  fi
  if ! grep -q '\[SKIP\] 2. verify GUI binary exists (build step failed (no reachable RCH workers); GUI binary unavailable)' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "missing_dependency_skip" "DEPENDENCY_SKIP_MISSING" "dependent GUI binary check did not skip after build failure"
    return
  fi
  if grep -q '\[FAIL\] 2. verify GUI binary exists' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "cascade_failure_present" "CASCADE_FAILURE_PRESENT" "dependent GUI binary check still failed instead of skipping"
    return
  fi
  if ! grep -q 'Summary: pass=0 fail=1 skip=6 total=7' "${stdout_file}"; then
    record_result "e2e_probe_failure_refuses_exec" "false" "unexpected_summary" "SUMMARY_MISMATCH" "probe-failure summary did not collapse to a single root-cause failure"
    return
  fi
  if ! jq -e '.data.results[0].status == "connection_failed"' "${probe_log}" >/dev/null; then
    record_result "e2e_probe_failure_refuses_exec" "false" "probe_artifact_missing" "PROBE_LOG_INVALID" "probe artifact missing connection_failed status"
    return
  fi
  record_result "e2e_probe_failure_refuses_exec" "true"
}

scenario_e2e_missing_x11_refuses_cargo() {
  local scenario_dir="${ARTIFACT_DIR}/e2e_missing_x11_refuses_cargo"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_missing_x11_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "e2e_missing_x11_refuses_cargo" "remote_prereq_guard" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh missing x11"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --skip-bundle >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "unexpected_success" "X11_GUARD_MISSING" "script unexpectedly succeeded"
    return
  fi
  if [[ ! -f "${marker_file}" ]]; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
    return
  fi
  if ! grep -q 'cargo build' "${marker_file}"; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "missing_authoritative_build" "CARGO_BUILD_MISSING" "script never ran the authoritative remote Cargo build"
    return
  fi
  if ! grep -q 'Package x11 was not found' "${stderr_file}"; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "missing_x11_diagnostic" "X11_DIAGNOSTIC_MISSING" "authoritative Cargo failure omitted the x11 diagnostic"
    return
  fi
  if ! grep -q 'Strict-remote frankenterm-gui build failed.' "${stdout_file}"; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "missing_x11_message" "X11_MESSAGE_MISSING" "missing explicit x11 prerequisite message"
    return
  fi
  if ! grep -q '\[SKIP\] 2. verify GUI binary exists (build step failed (strict-remote GUI cargo build returned non-zero); GUI binary unavailable)' "${stdout_file}"; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "missing_dependency_skip" "DEPENDENCY_SKIP_MISSING" "dependent GUI binary check did not skip after the Cargo failure"
    return
  fi
  if ! grep -q 'Summary: pass=0 fail=1 skip=6 total=7' "${stdout_file}"; then
    record_result "e2e_missing_x11_refuses_cargo" "false" "unexpected_summary" "SUMMARY_MISMATCH" "x11 preflight summary did not collapse to a single root-cause failure"
    return
  fi
  record_result "e2e_missing_x11_refuses_cargo" "true"
}

scenario_e2e_missing_gui_link_pkg_refuses_cargo() {
  local scenario_dir="${ARTIFACT_DIR}/e2e_missing_gui_link_pkg_refuses_cargo"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_missing_gui_link_pkg_rch "${mock_bin}" "${marker_file}" "xcb-image"

  emit_log "running" "e2e_missing_gui_link_pkg_refuses_cargo" "remote_prereq_guard" "none" "none" "${stdout_file}" "scripts/e2e_gui_bootstrap.sh missing xcb-image"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    LOG_DIR="${scenario_dir}/logs" \
    GUI_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/e2e_gui_bootstrap.sh" --skip-bundle >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "unexpected_success" "GUI_LINK_PKG_GUARD_MISSING" "script unexpectedly succeeded"
    return
  fi
  if [[ ! -f "${marker_file}" ]]; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
    return
  fi
  if ! grep -q 'cargo build' "${marker_file}"; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "missing_authoritative_build" "CARGO_BUILD_MISSING" "script never ran the authoritative remote Cargo build"
    return
  fi
  if ! grep -q 'Package xcb-image was not found' "${stderr_file}"; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "missing_gui_pkg_diagnostic" "GUI_LINK_PKG_DIAGNOSTIC_MISSING" "authoritative Cargo failure omitted the xcb-image diagnostic"
    return
  fi
  if ! grep -q 'Strict-remote frankenterm-gui build failed.' "${stdout_file}"; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "missing_gui_pkg_message" "GUI_LINK_PKG_MESSAGE_MISSING" "missing explicit xcb-image prerequisite message"
    return
  fi
  if ! grep -q '\[SKIP\] 2. verify GUI binary exists (build step failed (strict-remote GUI cargo build returned non-zero); GUI binary unavailable)' "${stdout_file}"; then
    record_result "e2e_missing_gui_link_pkg_refuses_cargo" "false" "missing_dependency_skip" "DEPENDENCY_SKIP_MISSING" "dependent GUI binary check did not skip after the Cargo failure"
    return
  fi
  record_result "e2e_missing_gui_link_pkg_refuses_cargo" "true"
}

scenario_bundle_skip_build_creates_structure() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_skip_build_creates_structure"
  local mock_bin="${scenario_dir}/mock-bin"
  local target_dir="${scenario_dir}/target"
  local output_dir="${scenario_dir}/output"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local codesign_log="${scenario_dir}/codesign.log"
  local app_bundle="${output_dir}/FrankenTerm.app"

  mkdir -p "${scenario_dir}" "${target_dir}/${BUNDLE_TARGET}/release-interactive" "${output_dir}"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/ft"
  write_codesign_mock "${mock_bin}" "${codesign_log}"

  emit_log "running" "bundle_skip_build_creates_structure" "skip_build_bundle" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh --skip-build"
  if env \
    PATH="${mock_bin}:${PATH}" \
    CARGO_TARGET_DIR="${target_dir}" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"; then
    for required_path in \
      "${app_bundle}/Contents/Info.plist" \
      "${app_bundle}/Contents/PkgInfo" \
      "${app_bundle}/Contents/Resources/ft.icns" \
      "${app_bundle}/Contents/Resources/frankenterm.toml" \
      "${app_bundle}/Contents/MacOS/frankenterm-gui" \
      "${app_bundle}/Contents/MacOS/frankenterm-mux-server" \
      "${app_bundle}/Contents/MacOS/frankenterm-pty-guardian" \
      "${app_bundle}/Contents/MacOS/ft"; do
      if [[ ! -e "${required_path}" ]]; then
        record_result "bundle_skip_build_creates_structure" "false" "missing_bundle_artifact" "BUNDLE_STRUCTURE_MISSING" "missing ${required_path}"
        return
      fi
    done
    record_result "bundle_skip_build_creates_structure" "true"
    return
  fi

  record_result "bundle_skip_build_creates_structure" "false" "bundle_creation_failed" "BUNDLE_SCRIPT_FAILED" "bundle script returned non-zero"
}

scenario_bundle_refuses_overwrite() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_refuses_overwrite"
  local mock_bin="${scenario_dir}/mock-bin"
  local target_dir="${scenario_dir}/target"
  local output_dir="${scenario_dir}/output"
  local stdout_first="${scenario_dir}/stdout-first.log"
  local stderr_first="${scenario_dir}/stderr-first.log"
  local stdout_second="${scenario_dir}/stdout-second.log"
  local stderr_second="${scenario_dir}/stderr-second.log"
  local codesign_log="${scenario_dir}/codesign.log"
  local app_bundle="${output_dir}/FrankenTerm.app"
  local rc

  mkdir -p "${scenario_dir}" "${target_dir}/${BUNDLE_TARGET}/release-interactive" "${output_dir}"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/ft"
  write_codesign_mock "${mock_bin}" "${codesign_log}"

  if ! env PATH="${mock_bin}:${PATH}" CARGO_TARGET_DIR="${target_dir}" "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_first}" 2>"${stderr_first}"; then
    record_result "bundle_refuses_overwrite" "false" "seed_bundle_failed" "SEED_BUNDLE_FAILED" "unable to create initial bundle"
    return
  fi
  if [[ ! -d "${app_bundle}" ]]; then
    record_result "bundle_refuses_overwrite" "false" "seed_bundle_missing" "SEED_BUNDLE_MISSING" "initial bundle missing"
    return
  fi

  emit_log "running" "bundle_refuses_overwrite" "overwrite_guard" "none" "none" "${stdout_second}" "bundle overwrite refusal"
  set +e
  env PATH="${mock_bin}:${PATH}" CARGO_TARGET_DIR="${target_dir}" "${ROOT_DIR}/scripts/create-macos-bundle.sh" --skip-build --output "${output_dir}" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_second}" 2>"${stderr_second}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_refuses_overwrite" "false" "overwrite_allowed" "OVERWRITE_GUARD_MISSING" "second bundle invocation unexpectedly succeeded"
    return
  fi
  if ! grep -q 'Error: app bundle already exists at' "${stdout_second}"; then
    record_result "bundle_refuses_overwrite" "false" "missing_overwrite_guard_message" "OVERWRITE_MESSAGE_MISSING" "overwrite refusal message missing"
    return
  fi
  record_result "bundle_refuses_overwrite" "true"
}

scenario_bundle_refuses_failed_codesign_verification() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_refuses_failed_codesign_verification"
  local mock_bin="${scenario_dir}/mock-bin"
  local target_dir="${scenario_dir}/target"
  local output_dir="${scenario_dir}/output"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}" "${target_dir}/${BUNDLE_TARGET}/release-interactive" "${output_dir}"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-gui"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-mux-server"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/frankenterm-pty-guardian"
  write_stub_binary "${target_dir}/${BUNDLE_TARGET}/release-interactive/ft"
  write_codesign_verification_failure_mock "${mock_bin}"

  emit_log "running" "bundle_refuses_failed_codesign_verification" "codesign_verify" "none" "none" "${stdout_file}" "strict deep codesign verification failure"
  set +e
  env \
    PATH="${mock_bin}:${PATH}" \
    CARGO_TARGET_DIR="${target_dir}" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" \
      --skip-build \
      --output "${output_dir}" \
      "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -ne 0 ]] && grep -q "failed strict deep codesign verification" "${stdout_file}"; then
    record_result "bundle_refuses_failed_codesign_verification" "true"
    return
  fi
  record_result "bundle_refuses_failed_codesign_verification" "false" "codesign_verify_false_green" "CODESIGN_VERIFY_FALSE_GREEN" "bundle did not fail closed when strict deep verification failed"
}

scenario_bundle_probe_failure_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_probe_failure_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_probe_failure_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "bundle_probe_failure_refuses_exec" "probe_guard" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh probe failure"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${scenario_dir}/output" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_probe_failure_refuses_exec" "false" "unexpected_success" "PROBE_GUARD_MISSING" "bundle script unexpectedly succeeded"
    return
  fi
  if [[ -f "${marker_file}" ]]; then
    record_result "bundle_probe_failure_refuses_exec" "false" "unexpected_exec" "RCH_EXEC_CALLED" "bundle script invoked mock exec path"
    return
  fi
  if ! grep -q 'Error: no reachable RCH workers detected; refusing local cargo fallback' "${stdout_file}"; then
    record_result "bundle_probe_failure_refuses_exec" "false" "missing_guardrail_message" "FAIL_OPEN_MESSAGE_MISSING" "bundle probe refusal missing"
    return
  fi
  record_result "bundle_probe_failure_refuses_exec" "true"
}

scenario_bundle_missing_x11_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_missing_x11_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_missing_x11_rch "${mock_bin}" "${marker_file}"

  emit_log "running" "bundle_missing_x11_refuses_exec" "remote_prereq_guard" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh missing x11"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${scenario_dir}/output" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_missing_x11_refuses_exec" "false" "unexpected_success" "X11_GUARD_MISSING" "bundle script unexpectedly succeeded"
    return
  fi
  if [[ ! -f "${marker_file}" ]]; then
    record_result "bundle_missing_x11_refuses_exec" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
    return
  fi
  if ! grep -q 'cargo build' "${marker_file}"; then
    record_result "bundle_missing_x11_refuses_exec" "false" "missing_authoritative_build" "CARGO_BUILD_MISSING" "bundle script never ran the authoritative remote Cargo build"
    return
  fi
  if ! grep -q 'Package x11 was not found' "${stderr_file}"; then
    record_result "bundle_missing_x11_refuses_exec" "false" "missing_x11_diagnostic" "X11_DIAGNOSTIC_MISSING" "authoritative Cargo failure omitted the x11 diagnostic"
    return
  fi
  if ! grep -q 'Error: strict-remote GUI/mux/CLI bundle build failed.' "${stdout_file}"; then
    record_result "bundle_missing_x11_refuses_exec" "false" "missing_x11_message" "X11_MESSAGE_MISSING" "bundle script missing explicit x11 prerequisite message"
    return
  fi
  record_result "bundle_missing_x11_refuses_exec" "true"
}

scenario_bundle_missing_gui_link_pkg_refuses_exec() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_missing_gui_link_pkg_refuses_exec"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"
  local rc

  mkdir -p "${scenario_dir}"
  write_missing_gui_link_pkg_rch "${mock_bin}" "${marker_file}" "xkbcommon-x11"

  emit_log "running" "bundle_missing_gui_link_pkg_refuses_exec" "remote_prereq_guard" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh missing xkbcommon-x11"
  set +e
  env \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${scenario_dir}/output" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"
  rc=$?
  set -e

  if [[ "${rc}" -eq 0 ]]; then
    record_result "bundle_missing_gui_link_pkg_refuses_exec" "false" "unexpected_success" "GUI_LINK_PKG_GUARD_MISSING" "bundle script unexpectedly succeeded"
    return
  fi
  if [[ ! -f "${marker_file}" ]]; then
    record_result "bundle_missing_gui_link_pkg_refuses_exec" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
    return
  fi
  if ! grep -q 'cargo build' "${marker_file}"; then
    record_result "bundle_missing_gui_link_pkg_refuses_exec" "false" "missing_authoritative_build" "CARGO_BUILD_MISSING" "bundle script never ran the authoritative remote Cargo build"
    return
  fi
  if ! grep -q 'Package xkbcommon-x11 was not found' "${stderr_file}"; then
    record_result "bundle_missing_gui_link_pkg_refuses_exec" "false" "missing_gui_pkg_diagnostic" "GUI_LINK_PKG_DIAGNOSTIC_MISSING" "authoritative Cargo failure omitted the xkbcommon-x11 diagnostic"
    return
  fi
  if ! grep -q 'Error: strict-remote GUI/mux/CLI bundle build failed.' "${stdout_file}"; then
    record_result "bundle_missing_gui_link_pkg_refuses_exec" "false" "missing_gui_pkg_message" "GUI_LINK_PKG_MESSAGE_MISSING" "bundle script missing explicit xkbcommon-x11 prerequisite message"
    return
  fi
  record_result "bundle_missing_gui_link_pkg_refuses_exec" "true"
}

scenario_bundle_build_uses_repo_relative_paths() {
  local scenario_dir="${ARTIFACT_DIR}/bundle_build_uses_repo_relative_paths"
  local mock_bin="${scenario_dir}/mock-bin"
  local marker_file="${scenario_dir}/rch-exec.log"
  local output_dir="${scenario_dir}/output"
  local stdout_file="${scenario_dir}/stdout.log"
  local stderr_file="${scenario_dir}/stderr.log"

  mkdir -p "${scenario_dir}" "${output_dir}"
  write_success_build_rch "${mock_bin}" "${marker_file}"
  write_codesign_mock "${mock_bin}" "${scenario_dir}/codesign.log"

  emit_log "running" "bundle_build_uses_repo_relative_paths" "remote_safe_build" "none" "none" "${stdout_file}" "scripts/create-macos-bundle.sh remote-safe paths"
  if env \
    PATH="${mock_bin}:${PATH}" \
    RCH_BIN="${mock_bin}/rch" \
    CARGO_TARGET_DIR="${scenario_dir}/target" \
    "${ROOT_DIR}/scripts/create-macos-bundle.sh" --output "${output_dir}" "${BROWSER_BUNDLE_ARGS[@]}" >"${stdout_file}" 2>"${stderr_file}"; then
    if [[ ! -f "${marker_file}" ]]; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "missing_exec_log" "RCH_EXEC_LOG_MISSING" "mock rch exec log missing"
      return
    fi
    if grep -Fq "${ROOT_DIR}/Cargo.toml" "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "absolute_manifest_path" "ABSOLUTE_MANIFEST_PATH" "bundle build invoked rch with host manifest path"
      return
    fi
    if grep -Fq "CARGO_TARGET_DIR=${ROOT_DIR}/" "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "absolute_target_dir" "ABSOLUTE_TARGET_DIR" "bundle build invoked rch with host target dir"
      return
    fi
    if ! grep -Fq -- '--manifest-path Cargo.toml' "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "missing_relative_manifest_path" "MANIFEST_PATH_NOT_RELATIVE" "bundle build omitted repo-relative manifest path"
      return
    fi
    if ! grep -Fq -- "--base ${SOURCE_REVISION}" "${marker_file}" ||
       ! grep -Fq -- '--clean-overlay' "${marker_file}" ||
       ! grep -Fq -- '--no-overlay' "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "missing_exact_source_fence" "EXACT_SOURCE_FENCE_MISSING" "bundle build omitted the exact committed-source RCH contract"
      return
    fi
    if grep -Fq -- '--source-content-receipt' "${marker_file}"; then
      record_result "bundle_build_uses_repo_relative_paths" "false" "incompatible_source_receipt" "RCH_ARGUMENT_CONFLICT" "bundle build combined mutually exclusive RCH source modes"
      return
    fi
    record_result "bundle_build_uses_repo_relative_paths" "true"
    return
  fi

  record_result "bundle_build_uses_repo_relative_paths" "false" "bundle_build_failed" "BUNDLE_BUILD_SCRIPT_FAILED" "bundle build path scenario returned non-zero"
}

main() {
  echo "=== GUI Bootstrap Contract E2E (ft-1memj.26) ==="
  echo "Artifacts: ${ARTIFACT_DIR}"
  emit_log "started" "suite" "script_init" "none" "none" "${LOG_FILE}" "RUN_ID=${RUN_ID}"

  require_cmd jq
  require_cmd python3
  require_cmd file
  test_native_dependency_closure
  prepare_browser_runtime_fixture

  scenario_dry_run_skips_rch
  scenario_e2e_probe_failure_refuses_exec
  scenario_e2e_missing_x11_refuses_cargo
  scenario_e2e_missing_gui_link_pkg_refuses_cargo
  scenario_bundle_skip_build_creates_structure
  scenario_bundle_refuses_overwrite
  scenario_bundle_refuses_failed_codesign_verification
  scenario_bundle_probe_failure_refuses_exec
  scenario_bundle_missing_x11_refuses_exec
  scenario_bundle_missing_gui_link_pkg_refuses_exec
  scenario_bundle_build_uses_repo_relative_paths

  echo ""
  echo "=== Summary ==="
  echo "  Total: ${TOTAL}  Pass: ${PASS}  Fail: ${FAIL}"
  echo "  Log: ${LOG_FILE}"

  emit_log "$([[ "${FAIL}" -eq 0 ]] && echo passed || echo failed)" \
    "suite" "script_end" "completed" "none" "${LOG_FILE}" \
    "total=${TOTAL},pass=${PASS},fail=${FAIL}"

  jq -cn \
    --arg test "gui_bootstrap_contract" \
    --argjson scenarios_pass "${PASS}" \
    --argjson scenarios_fail "${FAIL}" \
    --argjson total "${TOTAL}" \
    --arg log_file "${LOG_FILE}" \
    --arg artifact_dir "${ARTIFACT_DIR}" \
    '{
      test: $test,
      scenarios_pass: $scenarios_pass,
      scenarios_fail: $scenarios_fail,
      total: $total,
      log_file: $log_file,
      artifact_dir: $artifact_dir
    }'

  [[ "${FAIL}" -eq 0 ]]
}

main "$@"
