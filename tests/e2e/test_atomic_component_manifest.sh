#!/usr/bin/env bash
set -euo pipefail

# Static fixture matrix for ft-interactive-swarm-product-convergence-7xqz4.2.1.
# No FrankenTerm binary is executed.  The "executables" below are inert byte
# fixtures carrying the same compile-time marker as release binaries.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOL="$REPO_ROOT/scripts/atomic-component-manifest.sh"
SCHEMA="$REPO_ROOT/docs/json-schema/ft-atomic-component-manifest.json"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ft-atomic-manifest-matrix.XXXXXX")"

VERSION="0.13.0"
TARGET="aarch64-apple-darwin"
PROFILE="release"
FEATURE_CONTRACT="workspace-default-members-default-features-v1"
SOURCE_A="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
SOURCE_B="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

derive_build_id() {
  local source_revision="$1"
  local target="${2:-$TARGET}"
  local profile="${3:-$PROFILE}"
  local version="${4:-$VERSION}"
  bash "$TOOL" derive-build-id \
    --source-revision "$source_revision" \
    --version "$version" \
    --target "$target" \
    --profile "$profile" \
    --feature-contract "$FEATURE_CONTRACT"
}

BUILD_A="$(derive_build_id "$SOURCE_A")"
BUILD_B="$(derive_build_id "$SOURCE_B")"

if [[ ! "$BUILD_A" =~ ^[0-9a-f]{64}$ ]] || [[ "$BUILD_A" == "$BUILD_B" ]]; then
  echo "derive-build-id did not produce distinct canonical SHA-256 identities" >&2
  exit 1
fi

write_component() {
  local path="$1"
  local component="$2"
  local build_id="$3"
  local target="${4:-$TARGET}"
  local profile="${5:-$PROFILE}"
  local version="${6:-$VERSION}"
  mkdir -p "$(dirname "$path")"
  printf 'fixture-prefix\000FT_ATOMIC_COMPONENT_IDENTITY_V1:%s:%s:%s:%s:%s;\000fixture-suffix\n' \
    "$build_id" "$component" "$target" "$profile" "$version" > "$path"
  chmod 0755 "$path"
}

make_fixture() {
  local case_dir="$1"
  local gui_build="${2:-$BUILD_A}"
  local ft_build="${3:-$BUILD_A}"
  local mux_build="${4:-$BUILD_A}"
  local gui_target="${5:-$TARGET}"
  local gui_profile="${6:-$PROFILE}"
  local gui_version="${7:-$VERSION}"

  local package="$case_dir/package"
  local source="$case_dir/source"
  mkdir -p \
    "$package/bin" \
    "$package/defaults" \
    "$package/fonts" \
    "$package/schemas" \
    "$package/attestations" \
    "$source/defaults" \
    "$source/fonts" \
    "$source/schemas" \
    "$source/attestations" \
    "$source/protocol" \
    "$source/verifier"

  write_component \
    "$package/bin/frankenterm-gui" \
    "frankenterm-gui" \
    "$gui_build" \
    "$gui_target" \
    "$gui_profile" \
    "$gui_version"
  write_component "$package/bin/ft" "ft" "$ft_build"
  write_component "$package/bin/frankenterm-mux-server" "frankenterm-mux-server" "$mux_build"

  printf 'font = "Pragmasevka Nerd Font"\n' > "$source/defaults/frankenterm.toml"
  printf 'return { font = "Pragmasevka Nerd Font" }\n' > "$source/defaults/frankenterm.lua"
  printf 'inert-font-fixture-v1\n' > "$source/fonts/Pragmasevka-Regular.ttf"
  printf '{"schema":"fixture-v1"}\n' > "$source/schemas/attestations.json"
  printf '{"status":"retained-fixture"}\n' > "$source/attestations/proof.json"
  printf 'codec=50\ncodec_min=46\nrender_application=2\n' > "$source/protocol/versions.txt"

  cp "$source/defaults/frankenterm.toml" "$package/defaults/frankenterm.toml"
  cp "$source/defaults/frankenterm.lua" "$package/defaults/frankenterm.lua"
  cp "$source/fonts/Pragmasevka-Regular.ttf" "$package/fonts/Pragmasevka-Regular.ttf"
  cp "$source/schemas/attestations.json" "$package/schemas/attestations.json"
  cp "$source/attestations/proof.json" "$package/attestations/proof.json"
  cp "$TOOL" "$source/verifier/atomic-component-manifest.sh"
  cp "$source/verifier/atomic-component-manifest.sh" "$package/verify-components.sh"
  chmod 0755 "$package/verify-components.sh"
}

generate_fixture() {
  local case_dir="$1"
  local build_id="${2:-$BUILD_A}"
  local source_revision="${3:-$SOURCE_A}"
  local target="${4:-$TARGET}"
  local profile="${5:-$PROFILE}"
  local version="${6:-$VERSION}"
  local cli_entry="${7:-executable:cli:bin/ft:ft}"
  local output="${8:-$case_dir/manifest.json}"
  bash "$TOOL" generate \
    --root "$case_dir/package" \
    --source-root "$case_dir/source" \
    --output "$output" \
    --build-id "$build_id" \
    --source-revision "$source_revision" \
    --version "$version" \
    --target "$target" \
    --profile "$profile" \
    --feature-contract "$FEATURE_CONTRACT" \
    --entry executable:gui:bin/frankenterm-gui:frankenterm-gui \
    --entry "$cli_entry" \
    --entry executable:mux-server:bin/frankenterm-mux-server:frankenterm-mux-server \
    --entry config:default-toml:defaults/frankenterm.toml \
    --entry config:default-lua:defaults/frankenterm.lua \
    --entry schema:attestation-schema:schemas/attestations.json \
    --entry attestation:proof:attestations/proof.json \
    --entry verifier:offline-verifier:verify-components.sh \
    --tree font:fonts:fonts \
    --source-match defaults/frankenterm.toml=defaults/frankenterm.toml \
    --source-match defaults/frankenterm.lua=defaults/frankenterm.lua \
    --source-match fonts/Pragmasevka-Regular.ttf=fonts/Pragmasevka-Regular.ttf \
    --source-match schemas/attestations.json=schemas/attestations.json \
    --source-match attestations/proof.json=attestations/proof.json \
    --source-match verify-components.sh=verifier/atomic-component-manifest.sh \
    --input default.toml=defaults/frankenterm.toml \
    --input default.lua=defaults/frankenterm.lua \
    --input font.payload=fonts/Pragmasevka-Regular.ttf \
    --input schema.attestations=schemas/attestations.json \
    --input attestation.proof=attestations/proof.json \
    --input protocol.versions=protocol/versions.txt \
    --contract codec.version=50 \
    --contract codec.min-supported=46 \
    --contract render-application.version=2 \
    --contract storage.schema=32
}

verify_fixture() {
  local case_dir="$1"
  local manifest="${2:-$case_dir/manifest.json}"
  env \
    http_proxy=http://127.0.0.1:1 \
    https_proxy=http://127.0.0.1:1 \
    no_proxy=invalid \
    bash "$TOOL" verify \
      --root "$case_dir/package" \
      --manifest "$manifest"
}

expect_failure() {
  local code="$1"
  local log="$2"
  shift 2
  if "$@" >"$log.stdout" 2>"$log"; then
    echo "expected failure code $code, but command succeeded: $*" >&2
    exit 1
  fi
  if ! grep -Fq "\"code\": \"$code\"" "$log"; then
    echo "expected failure code $code; observed:" >&2
    sed -n '1,120p' "$log" >&2
    exit 1
  fi
}

# Clean package: generation and verification are deterministic, exact, and
# offline even with deliberately unusable proxy settings.
CLEAN="$TEST_ROOT/clean"
make_fixture "$CLEAN"
generate_fixture "$CLEAN" > "$CLEAN/generate.json"
verify_fixture "$CLEAN" > "$CLEAN/verify.json"
jq -e \
  --arg build "$BUILD_A" \
  '.schema_version == "ft.atomic_component_manifest.v1"
   and .identity.build_id == $build
   and .inventory.mode == "exact"
   and .inventory.file_count == 9
   and (.files | length) == 9
   and (.inputs | length) == 6
   and ([.files[].component] | map(select(. != null)) | sort)
       == ["frankenterm-gui", "frankenterm-mux-server", "ft"]' \
  "$CLEAN/manifest.json" >/dev/null
jq empty "$SCHEMA" "$CLEAN/manifest.json"
jq -e '
  .["$defs"].file.allOf
  | any(
      .if.properties.kind.const == "executable"
      and .then.required == ["component"]
    )
' "$SCHEMA" >/dev/null

NESTED_MANIFEST="$TEST_ROOT/nested-manifest-output"
make_fixture "$NESTED_MANIFEST"
mkdir "$NESTED_MANIFEST/package/metadata"
generate_fixture \
  "$NESTED_MANIFEST" \
  "$BUILD_A" \
  "$SOURCE_A" \
  "$TARGET" \
  "$PROFILE" \
  "$VERSION" \
  executable:cli:bin/ft:ft \
  "$NESTED_MANIFEST/package/metadata/manifest.json" >/dev/null
verify_fixture \
  "$NESTED_MANIFEST" \
  "$NESTED_MANIFEST/package/metadata/manifest.json" >/dev/null

# Re-running generation cannot overwrite an existing authority-bearing file.
expect_failure output_exists "$CLEAN/output-exists.log" generate_fixture "$CLEAN"

UNMARKED_EXECUTABLE="$TEST_ROOT/unmarked-executable-generator"
make_fixture "$UNMARKED_EXECUTABLE"
expect_failure executable_component_required "$UNMARKED_EXECUTABLE/error.log" \
  generate_fixture \
    "$UNMARKED_EXECUTABLE" \
    "$BUILD_A" \
    "$SOURCE_A" \
    "$TARGET" \
    "$PROFILE" \
    "$VERSION" \
    executable:cli:bin/ft

EXECUTABLE_TREE="$TEST_ROOT/executable-tree-generator"
make_fixture "$EXECUTABLE_TREE"
expect_failure executable_tree_forbidden "$EXECUTABLE_TREE/error.log" \
  bash "$TOOL" generate \
    --root "$EXECUTABLE_TREE/package" \
    --source-root "$EXECUTABLE_TREE/source" \
    --output "$EXECUTABLE_TREE/manifest.json" \
    --build-id "$BUILD_A" \
    --source-revision "$SOURCE_A" \
    --version "$VERSION" \
    --target "$TARGET" \
    --profile "$PROFILE" \
    --feature-contract "$FEATURE_CONTRACT" \
    --tree executable:components:bin

MISSING_MARKER="$TEST_ROOT/missing-component-marker"
make_fixture "$MISSING_MARKER"
printf 'inert executable without an atomic identity marker\n' > "$MISSING_MARKER/package/bin/ft"
chmod 0755 "$MISSING_MARKER/package/bin/ft"
expect_failure component_identity_missing "$MISSING_MARKER/error.log" \
  generate_fixture "$MISSING_MARKER"

# Stale and mixed executable matrices report the expected identity, the found
# identity, and the rebuild remedy.  Hashing the mixed package is forbidden.
STALE="$TEST_ROOT/stale-gui"
make_fixture "$STALE" "$BUILD_B" "$BUILD_A" "$BUILD_A"
expect_failure component_identity_mismatch "$STALE/error.log" generate_fixture "$STALE"
grep -Fq "$BUILD_A" "$STALE/error.log"
grep -Fq "$BUILD_B" "$STALE/error.log"
grep -Fq 'rebuild every FrankenTerm component' "$STALE/error.log"

MIXED="$TEST_ROOT/mixed-cli-mux"
make_fixture "$MIXED" "$BUILD_A" "$BUILD_B" "$BUILD_B"
expect_failure component_identity_mismatch "$MIXED/error.log" generate_fixture "$MIXED"
grep -Fq "$BUILD_A" "$MIXED/error.log"
grep -Fq "$BUILD_B" "$MIXED/error.log"

STALE_CLI="$TEST_ROOT/stale-cli-only"
make_fixture "$STALE_CLI" "$BUILD_A" "$BUILD_B" "$BUILD_A"
expect_failure component_identity_mismatch "$STALE_CLI/error.log" generate_fixture "$STALE_CLI"

STALE_MUX="$TEST_ROOT/stale-mux-only"
make_fixture "$STALE_MUX" "$BUILD_A" "$BUILD_A" "$BUILD_B"
expect_failure component_identity_mismatch "$STALE_MUX/error.log" generate_fixture "$STALE_MUX"

MULTIPLE_MARKERS="$TEST_ROOT/multiple-component-markers"
make_fixture "$MULTIPLE_MARKERS"
printf 'FT_ATOMIC_COMPONENT_IDENTITY_V1:%s:ft:%s:%s:%s;\n' \
  "$BUILD_B" "$TARGET" "$PROFILE" "$VERSION" >> "$MULTIPLE_MARKERS/package/bin/ft"
expect_failure component_identity_mismatch "$MULTIPLE_MARKERS/error.log" generate_fixture "$MULTIPLE_MARKERS"
grep -Fq "$BUILD_A" "$MULTIPLE_MARKERS/error.log"
grep -Fq "$BUILD_B" "$MULTIPLE_MARKERS/error.log"

# Version/profile/target changes are part of the marker, not advisory metadata.
WRONG_TARGET="$TEST_ROOT/wrong-target"
make_fixture "$WRONG_TARGET" "$BUILD_A" "$BUILD_A" "$BUILD_A" x86_64-apple-darwin
expect_failure component_identity_mismatch "$WRONG_TARGET/error.log" generate_fixture "$WRONG_TARGET"

WRONG_PROFILE="$TEST_ROOT/wrong-profile"
make_fixture "$WRONG_PROFILE" "$BUILD_A" "$BUILD_A" "$BUILD_A" "$TARGET" release-perf
expect_failure component_identity_mismatch "$WRONG_PROFILE/error.log" generate_fixture "$WRONG_PROFILE"

WRONG_VERSION="$TEST_ROOT/wrong-version"
make_fixture "$WRONG_VERSION" "$BUILD_A" "$BUILD_A" "$BUILD_A" "$TARGET" "$PROFILE" 0.12.0
expect_failure component_identity_mismatch "$WRONG_VERSION/error.log" generate_fixture "$WRONG_VERSION"

# A manifest cannot claim an arbitrary build id inconsistent with its declared
# source/profile/target/version/feature tuple.
LIE="$TEST_ROOT/identity-lie"
make_fixture "$LIE" "$BUILD_B" "$BUILD_B" "$BUILD_B"
expect_failure build_identity_derivation_mismatch "$LIE/error.log" \
  generate_fixture "$LIE" "$BUILD_B" "$SOURCE_A"

# Defaults, fonts, schemas, and attestations must match the declared source
# input bytes at packaging time.
for asset_case in default font schema attestation; do
  CASE="$TEST_ROOT/source-mismatch-$asset_case"
  make_fixture "$CASE"
  case "$asset_case" in
    default) printf '\nremote_host = "forbidden"\n' >> "$CASE/package/defaults/frankenterm.toml" ;;
    font) printf 'corrupt-font\n' >> "$CASE/package/fonts/Pragmasevka-Regular.ttf" ;;
    schema) printf 'schema-drift\n' >> "$CASE/package/schemas/attestations.json" ;;
    attestation) printf 'attestation-drift\n' >> "$CASE/package/attestations/proof.json" ;;
  esac
  expect_failure source_asset_mismatch "$CASE/error.log" generate_fixture "$CASE"
done

# Uncatalogued companions and missing declared files fail exact-inventory
# generation rather than being silently blessed.
EXTRA="$TEST_ROOT/extra-companion"
make_fixture "$EXTRA"
printf 'stale-companion\n' > "$EXTRA/package/bin/ft.old"
chmod 0755 "$EXTRA/package/bin/ft.old"
expect_failure package_inventory_mismatch "$EXTRA/error.log" generate_fixture "$EXTRA"
grep -Fq 'bin/ft.old' "$EXTRA/error.log"

MISSING="$TEST_ROOT/missing-component"
make_fixture "$MISSING"
mv "$MISSING/package/bin/frankenterm-mux-server" "$MISSING/mux-not-in-package"
expect_failure package_inventory_mismatch "$MISSING/error.log" generate_fixture "$MISSING"
grep -Fq 'bin/frankenterm-mux-server' "$MISSING/error.log"

SYMLINK="$TEST_ROOT/symlink-component"
make_fixture "$SYMLINK"
ln -s ../defaults/frankenterm.toml "$SYMLINK/package/bin/config-link"
expect_failure non_regular_inventory_entry "$SYMLINK/error.log" generate_fixture "$SYMLINK"

# Descriptor anchoring must remain fail-closed when a catalogued pathname or
# one of its parents is swapped after the regular-file descriptor is open.
# The test-only hook performs the swap at that exact boundary; a pathname-only
# verifier would silently hash the wrong object or bless a changed tree.
FILE_SWAP="$TEST_ROOT/file-swap-after-open"
make_fixture "$FILE_SWAP"
generate_fixture "$FILE_SWAP" >/dev/null
write_component "$FILE_SWAP/replacement-ft" "ft" "$BUILD_B"
expect_failure file_changed_during_read "$FILE_SWAP/error.log" \
  env \
    FT_ATOMIC_MANIFEST_TEST_MODE=1 \
    FT_ATOMIC_MANIFEST_TEST_SWAP_FILE_AFTER_OPEN=bin/ft \
    FT_ATOMIC_MANIFEST_TEST_SWAP_REPLACEMENT="$FILE_SWAP/replacement-ft" \
    bash "$TOOL" verify \
      --root "$FILE_SWAP/package" \
      --manifest "$FILE_SWAP/manifest.json"
grep -Fq 'bin/ft' "$FILE_SWAP/error.log"

PARENT_SWAP="$TEST_ROOT/parent-swap-after-open"
make_fixture "$PARENT_SWAP"
generate_fixture "$PARENT_SWAP" >/dev/null
mkdir -p "$PARENT_SWAP/replacement-defaults"
printf 'return { font = "replacement" }\n' > "$PARENT_SWAP/replacement-defaults/frankenterm.lua"
printf 'font = "replacement"\n' > "$PARENT_SWAP/replacement-defaults/frankenterm.toml"
expect_failure package_inventory_changed "$PARENT_SWAP/error.log" \
  env \
    FT_ATOMIC_MANIFEST_TEST_MODE=1 \
    FT_ATOMIC_MANIFEST_TEST_SWAP_PARENT_AFTER_OPEN=defaults/frankenterm.toml \
    FT_ATOMIC_MANIFEST_TEST_SWAP_REPLACEMENT="$PARENT_SWAP/replacement-defaults" \
    bash "$TOOL" verify \
      --root "$PARENT_SWAP/package" \
      --manifest "$PARENT_SWAP/manifest.json"
grep -Fq '.ft-atomic-opened-defaults' "$PARENT_SWAP/error.log"

PRECOMMIT_SWAP="$TEST_ROOT/file-swap-after-precommit-scan"
make_fixture "$PRECOMMIT_SWAP"
write_component "$PRECOMMIT_SWAP/replacement-ft" "ft" "$BUILD_B"
FT_ATOMIC_MANIFEST_TEST_MODE=1 \
FT_ATOMIC_MANIFEST_TEST_SWAP_AFTER_PRECOMMIT_SCAN=bin/ft \
FT_ATOMIC_MANIFEST_TEST_SWAP_REPLACEMENT="$PRECOMMIT_SWAP/replacement-ft" \
  expect_failure package_inventory_changed "$PRECOMMIT_SWAP/error.log" \
    generate_fixture "$PRECOMMIT_SWAP"
grep -Fq '.ft-atomic-precommit-ft' "$PRECOMMIT_SWAP/error.log"
if [ ! -f "$PRECOMMIT_SWAP/manifest.json" ]; then
  echo "precommit race did not reach the post-output authority recheck" >&2
  exit 1
fi
expect_failure non_regular_inventory_entry "$PRECOMMIT_SWAP/verify-error.log" \
  verify_fixture "$PRECOMMIT_SWAP"

# Post-generation corruption, removal, and additions all fail offline replay.
CORRUPT="$TEST_ROOT/corrupt-after-generate"
make_fixture "$CORRUPT"
generate_fixture "$CORRUPT" >/dev/null
printf 'corruption\n' >> "$CORRUPT/package/bin/ft"
expect_failure file_content_mismatch "$CORRUPT/error.log" verify_fixture "$CORRUPT"

REMOVED="$TEST_ROOT/removed-after-generate"
make_fixture "$REMOVED"
generate_fixture "$REMOVED" >/dev/null
mv "$REMOVED/package/fonts/Pragmasevka-Regular.ttf" "$REMOVED/font-removed"
expect_failure package_inventory_mismatch "$REMOVED/error.log" verify_fixture "$REMOVED"

ADDED="$TEST_ROOT/added-after-generate"
make_fixture "$ADDED"
generate_fixture "$ADDED" >/dev/null
printf 'uncatalogued\n' > "$ADDED/package/defaults/extra.toml"
expect_failure package_inventory_mismatch "$ADDED/error.log" verify_fixture "$ADDED"

MODE="$TEST_ROOT/mode-after-generate"
make_fixture "$MODE"
generate_fixture "$MODE" >/dev/null
chmod 0644 "$MODE/package/bin/ft"
expect_failure file_mode_mismatch "$MODE/error.log" verify_fixture "$MODE"

MANIFEST_SYMLINK="$TEST_ROOT/manifest-symlink"
make_fixture "$MANIFEST_SYMLINK"
generate_fixture "$MANIFEST_SYMLINK" >/dev/null
mv "$MANIFEST_SYMLINK/manifest.json" "$MANIFEST_SYMLINK/manifest-real.json"
ln -s manifest-real.json "$MANIFEST_SYMLINK/manifest.json"
expect_failure symlink_rejected "$MANIFEST_SYMLINK/error.log" verify_fixture "$MANIFEST_SYMLINK"

# Schema and canonical-content tampering fail before package files are trusted.
SCHEMA_TAMPER="$TEST_ROOT/schema-tamper"
make_fixture "$SCHEMA_TAMPER"
generate_fixture "$SCHEMA_TAMPER" >/dev/null
python3 - "$SCHEMA_TAMPER/manifest.json" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    value = json.load(handle)
value["schema_version"] = "ft.atomic_component_manifest.v999"
with open(path, "w", encoding="utf-8") as handle:
    json.dump(value, handle, sort_keys=True)
    handle.write("\n")
PY
expect_failure schema_version_mismatch "$SCHEMA_TAMPER/error.log" verify_fixture "$SCHEMA_TAMPER"

ID_TAMPER="$TEST_ROOT/id-tamper"
make_fixture "$ID_TAMPER"
generate_fixture "$ID_TAMPER" >/dev/null
python3 - "$ID_TAMPER/manifest.json" <<'PY'
import json
import sys
path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    value = json.load(handle)
value["manifest_id"] = "sha256:" + ("0" * 64)
with open(path, "w", encoding="utf-8") as handle:
    json.dump(value, handle, sort_keys=True)
    handle.write("\n")
PY
expect_failure manifest_id_mismatch "$ID_TAMPER/error.log" verify_fixture "$ID_TAMPER"

DUPLICATE_KEY="$TEST_ROOT/duplicate-json-key"
make_fixture "$DUPLICATE_KEY"
generate_fixture "$DUPLICATE_KEY" >/dev/null
python3 - "$DUPLICATE_KEY/manifest.json" <<'PY'
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    payload = handle.read()
needle = '  "schema_version": "ft.atomic_component_manifest.v1",\n'
if payload.count(needle) != 1:
    raise SystemExit("schema_version fixture line is not unique")
payload = payload.replace(needle, needle + needle, 1)
with open(path, "w", encoding="utf-8") as handle:
    handle.write(payload)
PY
expect_failure duplicate_json_key "$DUPLICATE_KEY/error.log" verify_fixture "$DUPLICATE_KEY"

MALFORMED_KIND="$TEST_ROOT/malformed-kind-type"
make_fixture "$MALFORMED_KIND"
generate_fixture "$MALFORMED_KIND" >/dev/null
python3 - "$MALFORMED_KIND/manifest.json" <<'PY'
import hashlib
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    value = json.load(handle)
value["files"][0]["kind"] = []
without_id = dict(value)
without_id.pop("manifest_id", None)
canonical = json.dumps(
    without_id,
    ensure_ascii=False,
    separators=(",", ":"),
    sort_keys=True,
).encode("utf-8")
value["manifest_id"] = "sha256:" + hashlib.sha256(canonical).hexdigest()
with open(path, "w", encoding="utf-8") as handle:
    json.dump(value, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
PY
expect_failure invalid_file_kind "$MALFORMED_KIND/error.log" verify_fixture "$MALFORMED_KIND"

UNMARKED_MANIFEST="$TEST_ROOT/unmarked-executable-manifest"
make_fixture "$UNMARKED_MANIFEST"
generate_fixture "$UNMARKED_MANIFEST" >/dev/null
python3 - "$UNMARKED_MANIFEST/manifest.json" <<'PY'
import hashlib
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    value = json.load(handle)
for record in value["files"]:
    if record.get("kind") == "executable":
        record.pop("component", None)
        break
else:
    raise SystemExit("fixture contains no executable record")
without_id = dict(value)
without_id.pop("manifest_id", None)
canonical = json.dumps(
    without_id,
    ensure_ascii=False,
    separators=(",", ":"),
    sort_keys=True,
).encode("utf-8")
value["manifest_id"] = "sha256:" + hashlib.sha256(canonical).hexdigest()
with open(path, "w", encoding="utf-8") as handle:
    json.dump(value, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
PY
expect_failure executable_component_required "$UNMARKED_MANIFEST/error.log" \
  verify_fixture "$UNMARKED_MANIFEST"

# Attestation input drift changes the authority-bearing manifest id even when
# packaged component names remain the same.
ATTEST_A="$TEST_ROOT/attestation-a"
ATTEST_B="$TEST_ROOT/attestation-b"
make_fixture "$ATTEST_A"
make_fixture "$ATTEST_B"
printf '{"status":"different-retained-proof"}\n' > "$ATTEST_B/source/attestations/proof.json"
cp "$ATTEST_B/source/attestations/proof.json" "$ATTEST_B/package/attestations/proof.json"
generate_fixture "$ATTEST_A" >/dev/null
generate_fixture "$ATTEST_B" >/dev/null
MANIFEST_A="$(jq -r .manifest_id "$ATTEST_A/manifest.json")"
MANIFEST_B="$(jq -r .manifest_id "$ATTEST_B/manifest.json")"
if [[ "$MANIFEST_A" == "$MANIFEST_B" ]]; then
  echo "attestation input drift did not change manifest identity" >&2
  exit 1
fi

printf 'FT_ATOMIC_COMPONENT_MANIFEST_MATRIX_SUCCESS root=%s build_id=%s manifest_id=%s\n' \
  "$TEST_ROOT" "$BUILD_A" "$(jq -r .manifest_id "$CLEAN/manifest.json")"
