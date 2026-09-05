#!/usr/bin/env bash
# scripts/release/verify-release.sh — verify the exact signed DSR release set.
#
# DSR's build manifest is local, unsigned build-plan authority. It binds the
# version, source revision, successful build targets, primary names, hashes,
# and sizes. It is deliberately not uploaded: making it a release asset would
# make the strict asset set self-referential. DSR creates the checksum and
# Minisign sidecars during strict release preflight without rewriting that
# manifest.
#
# Usage:
#   scripts/release/verify-release.sh <version> [--manifest <path>]
#       Verify a published release. Without --manifest, use DSR's standard
#       local build-manifest path for the version.
#
#   scripts/release/verify-release.sh --manifest <path> --assets-dir <dir>
#       Offline verification of the same closed asset set. The local manifest
#       may be in <dir>, but is not counted as a release asset.
#
#   Add --attestation-bundle <path> --release-policy <path> to either mode
#       to authenticate a separate attestation against an operator-owned
#       policy and bind it to this DSR source, tag and four native targets.
#       FT_ATTESTATION_RELEASE_POLICY may supply the policy path. Both inputs
#       are required together and remain outside the exact release asset set.
#
# Checks, all fail-closed:
#   1. exact successful DSR build plan for four native targets plus the app;
#   2. exact 17-file release set: five artifacts and their .sha256/.minisig
#      sidecars, plus SHA256SUMS and SHA256SUMS.minisig (no extras);
#   3. bounded regular-file sizes, manifest hashes/sizes, and exact checksum
#      sidecar contents;
#   4. every Minisign signature against the tracked release/minisign.pub key;
#   5. archive member inventories accepted by the production installer;
#   6. online release/tag identity bound to the manifest source commit.
#
# This preserves the v0.15.1 negative contract: its four-artifact unsigned
# build output cannot satisfy the current strict five-artifact release plan.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
PUBKEY="$ROOT/release/minisign.pub"
OWNER_REPO="${FT_RELEASE_REPO:-Dicklesworthstone/frankenterm}"
DSR_STATE_ROOT="${DSR_STATE_DIR:-${XDG_STATE_HOME:-${HOME:?HOME is required}/.local/state}/dsr}"

VERSION=""
MANIFEST=""
ASSETS_DIR=""
ATTESTATION_BUNDLE=""
RELEASE_POLICY="${FT_ATTESTATION_RELEASE_POLICY:-}"

usage() {
  sed -n '2,36p' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest)
      [[ $# -ge 2 ]] || { echo "--manifest needs a path" >&2; exit 2; }
      MANIFEST="$2"
      shift 2
      ;;
    --assets-dir)
      [[ $# -ge 2 ]] || { echo "--assets-dir needs a directory" >&2; exit 2; }
      ASSETS_DIR="$2"
      shift 2
      ;;
    --attestation-bundle|--release-policy)
      [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { echo "$1 needs a path" >&2; exit 2; }
      if [[ "$1" == --attestation-bundle ]]; then ATTESTATION_BUNDLE="$2"; else RELEASE_POLICY="$2"; fi
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "unknown option: $1" >&2
      exit 2
      ;;
    *)
      [[ -z "$VERSION" ]] || { echo "only one version may be specified" >&2; exit 2; }
      VERSION="$1"
      shift
      ;;
  esac
done

if [[ -n "$ATTESTATION_BUNDLE" && -z "$RELEASE_POLICY" || -z "$ATTESTATION_BUNDLE" && -n "$RELEASE_POLICY" ]]; then
  echo "--attestation-bundle and --release-policy (or FT_ATTESTATION_RELEASE_POLICY) are required together" >&2
  exit 2
fi
if [[ -n "$VERSION" && -n "$ASSETS_DIR" ]]; then
  echo "--assets-dir is only valid for offline --manifest verification" >&2
  exit 2
fi
if [[ -z "$VERSION" && ( -z "$MANIFEST" || -z "$ASSETS_DIR" ) ]]; then
  echo "usage: $0 <version> [--manifest <path>] | --manifest <path> --assets-dir <dir>" >&2
  exit 2
fi

for required_command in jq minisign cmp python3; do
  command -v "$required_command" >/dev/null 2>&1 || {
    echo "$required_command is required" >&2
    exit 2
  }
done

if command -v shasum >/dev/null 2>&1; then
  SHA256_COMMAND="shasum"
elif command -v sha256sum >/dev/null 2>&1; then
  SHA256_COMMAND="sha256sum"
else
  echo "shasum or sha256sum is required" >&2
  exit 2
fi

fail=0
note() { printf '%s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; fail=$((fail + 1)); }
good() { printf 'ok   %s\n' "$*"; }

finish_failed() {
  echo "release verification: $fail failure(s) — do not publish or announce"
  exit 1
}

regular_file() {
  [[ -f "$1" && ! -L "$1" ]]
}

sha256_file() {
  if [[ "$SHA256_COMMAND" == "shasum" ]]; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

file_size() {
  if stat -f '%z' "$1" >/dev/null 2>&1; then
    stat -f '%z' "$1"
  else
    stat -c '%s' "$1"
  fi
}

max_size_for_asset() {
  case "$1" in
    FrankenTerm-darwin-arm64.app.tar.xz) printf '%s\n' 4294967296 ;;
    *.tar.xz|*.zip) printf '%s\n' 1073741824 ;;
    *.sha256) printf '%s\n' 4096 ;;
    *.minisig) printf '%s\n' 65536 ;;
    SHA256SUMS) printf '%s\n' 65536 ;;
    *) return 1 ;;
  esac
}

if ! regular_file "$PUBKEY"; then
  echo "tracked Minisign public key is missing or not a regular file: $PUBKEY" >&2
  exit 2
fi

TAG=""
if [[ -n "$VERSION" ]]; then
  if [[ ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version must be a stable semantic version such as v0.15.2 or 0.15.2" >&2
    exit 2
  fi
  TAG="v${VERSION#v}"
  if [[ -z "$MANIFEST" ]]; then
    standard_manifest="$DSR_STATE_ROOT/artifacts/frankenterm-$TAG/frankenterm-$TAG-manifest.json"
    alternate_manifest="$DSR_STATE_ROOT/artifacts/frankenterm-$TAG-manifest.json"
    if regular_file "$standard_manifest"; then
      MANIFEST="$standard_manifest"
    elif regular_file "$alternate_manifest"; then
      MANIFEST="$alternate_manifest"
    else
      bad "local DSR build manifest not found: $standard_manifest"
      finish_failed
    fi
  fi
fi

if ! regular_file "$MANIFEST"; then
  bad "manifest is missing, a symlink, or not a regular file: $MANIFEST"
  finish_failed
fi
manifest_size="$(file_size "$MANIFEST")"
if [[ ! "$manifest_size" =~ ^[0-9]+$ || "$manifest_size" -le 0 || "$manifest_size" -gt 1048576 ]]; then
  bad "manifest is empty or exceeds the 1 MiB local authority bound"
  finish_failed
fi
if ! python3 - "$MANIFEST" <<'PY'
import json
import sys

def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result

with open(sys.argv[1], "rb") as source:
    json.load(source, object_pairs_hook=unique_object)
PY
then
  bad "manifest is not valid duplicate-free JSON: $MANIFEST"
  finish_failed
fi
MANIFEST_FILE_SHA="$(sha256_file "$MANIFEST")"

if [[ -z "$TAG" ]]; then
  TAG="$(jq -r '.version // empty' "$MANIFEST")"
  if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    bad "manifest version is not a stable v-prefixed semantic version"
    finish_failed
  fi
fi

if ! jq -e \
  --arg tag "$TAG" '
    .tool == "frankenterm" and
    .version == $tag and
    (.source | type == "object") and
    .source.git_ref == $tag and
    (.source.git_sha | type == "string" and test("^[0-9a-f]{40}$")) and
    .source.dependencies == [] and
    .status == "success" and
    (.summary | type == "object") and
    .summary.total == 4 and
    .summary.success == 4 and
    .summary.failed == 0 and
    (.artifacts | type == "array") and
    (.artifacts | length == 5) and
    ([.artifacts[].name] | unique | length) == 5 and
    ([.artifacts[] | {target: .target, name: .name}] | sort_by(.target, .name)) ==
      ([
        {target: "additional", name: "FrankenTerm-darwin-arm64.app.tar.xz"},
        {target: "darwin/arm64", name: "ft-darwin-arm64.tar.xz"},
        {target: "linux/amd64", name: "ft-linux-amd64.tar.xz"},
        {target: "linux/arm64", name: "ft-linux-arm64.tar.xz"},
        {target: "windows/amd64", name: "ft-windows-amd64.zip"}
      ] | sort_by(.target, .name)) and
    all(.artifacts[];
      (.sha256 | type == "string" and test("^[0-9a-f]{64}$")) and
      (.size_bytes | type == "number" and floor == . and . > 0)
    )
  ' "$MANIFEST" >/dev/null 2>&1; then
  bad "manifest does not satisfy the exact successful DSR release plan for $TAG"
  finish_failed
fi

MANIFEST_PARENT="$(cd "$(dirname "$MANIFEST")" && pwd -P)"
MANIFEST="$MANIFEST_PARENT/$(basename "$MANIFEST")"
MANIFEST_SHA="$(jq -r '.source.git_sha' "$MANIFEST")"
good "local unsigned build manifest binds $TAG to $MANIFEST_SHA and all four targets plus app"

if [[ -n "$ATTESTATION_BUNDLE" ]]; then
  # Freeze the exact public inputs before authentication and cross-binding.
  # They are separate evidence, never extra entries in the 17-asset inventory.
  for input in "$ATTESTATION_BUNDLE" "$RELEASE_POLICY"; do
    if ! regular_file "$input"; then
      bad "requested attestation input is missing, a symlink, or not a regular file: $input"
      finish_failed
    fi
    input_size="$(file_size "$input")"
    if [[ ! "$input_size" =~ ^[0-9]+$ || "$input_size" -le 0 || "$input_size" -gt 1048576 ]]; then
      bad "requested attestation input is empty or exceeds the 1 MiB bound"
      finish_failed
    fi
  done
  ATTESTATION_EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ft-release-attestation.XXXXXX")" || exit 2
  note "attestation verification evidence retained at: $ATTESTATION_EVIDENCE_DIR"
  if ! cp "$ATTESTATION_BUNDLE" "$ATTESTATION_EVIDENCE_DIR/bundle.json" || \
      ! cp "$RELEASE_POLICY" "$ATTESTATION_EVIDENCE_DIR/policy.json"; then
    bad "could not retain requested attestation inputs"
    finish_failed
  fi
  if ! bash "$ROOT/scripts/attestation-verify.sh" \
      "$ATTESTATION_EVIDENCE_DIR/bundle.json" --json \
      --release-policy "$ATTESTATION_EVIDENCE_DIR/policy.json" \
      > "$ATTESTATION_EVIDENCE_DIR/verdict.json" 2> "$ATTESTATION_EVIDENCE_DIR/verifier.stderr"; then
    cat "$ATTESTATION_EVIDENCE_DIR/verifier.stderr" >&2
    bad "requested release attestation failed authentication; see $ATTESTATION_EVIDENCE_DIR/verdict.json"
    finish_failed
  fi
  if ! jq -e --arg sha "$(sha256_file "$ATTESTATION_EVIDENCE_DIR/bundle.json")" '
    .ok == true and .verdict == "pass" and .verification_mode == "release_policy"
    and .publisher_authenticated == true and .bundle_sha256 == $sha
  ' "$ATTESTATION_EVIDENCE_DIR/verdict.json" >/dev/null; then
    bad "attestation verifier did not authenticate the retained bundle under release policy"
    finish_failed
  fi
  if ! jq -e --arg tag "$TAG" --arg sha "$MANIFEST_SHA" --slurpfile manifest "$MANIFEST" '
    {"darwin/arm64":"aarch64-apple-darwin", "linux/amd64":"x86_64-unknown-linux-gnu",
     "linux/arm64":"aarch64-unknown-linux-gnu", "windows/amd64":"x86_64-pc-windows-msvc"} as $triples
    | ([$manifest[0].artifacts[] | select(.target != "additional") | $triples[.target]] | sort) as $targets
    | .release.tag == $tag and .release.version == ($tag | ltrimstr("v"))
      and .release.channel == "stable" and .git.commit == $sha
      and .build.profile == "release-interactive" and (.build.targets | sort) == $targets
  ' "$ATTESTATION_EVIDENCE_DIR/bundle.json" >/dev/null; then
    bad "authenticated attestation does not match DSR release tag, source, profile or native targets"
    finish_failed
  fi
  good "authenticated attestation matches DSR tag, source, release-interactive profile and four native targets"
else
  note "verification scope: artifact-set verification only; attestation not requested"
fi

declare -a BASE_ASSETS=()
while IFS= read -r name; do
  [[ -n "$name" ]] && BASE_ASSETS+=("$name")
done < <(jq -r '.artifacts | sort_by(.name)[] | .name' "$MANIFEST")

declare -a EXPECTED_ASSETS=()
for name in "${BASE_ASSETS[@]}"; do
  EXPECTED_ASSETS+=("$name" "$name.sha256" "$name.minisig")
done
EXPECTED_ASSETS+=("SHA256SUMS" "SHA256SUMS.minisig")

is_expected_asset() {
  local candidate="$1"
  local expected
  for expected in "${EXPECTED_ASSETS[@]}"; do
    [[ "$candidate" == "$expected" ]] && return 0
  done
  return 1
}

verify_exact_inventory() {
  local directory="$1"
  local directory_abs entry name expected
  if [[ ! -d "$directory" || -L "$directory" ]]; then
    bad "assets directory is missing, a symlink, or not a directory: $directory"
    return
  fi
  directory_abs="$(cd "$directory" && pwd -P)"
  while IFS= read -r entry; do
    if [[ "$entry" == "$MANIFEST" ]]; then
      continue
    fi
    name="${entry#"$directory_abs"/}"
    if [[ "$name" == */* ]] || ! is_expected_asset "$name"; then
      bad "unexpected local release-set entry: $name"
    fi
  done < <(find "$directory_abs" -mindepth 1 -maxdepth 1 -print)

  for expected in "${EXPECTED_ASSETS[@]}"; do
    if ! regular_file "$directory_abs/$expected"; then
      bad "required release asset is missing, a symlink, or not a regular file: $expected"
    fi
  done
}

verify_archive_inventory() {
  local name="$1"
  local archive_kind manifest_name
  case "$name" in
    ft-darwin-arm64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-darwin-arm64.component-manifest.json"
      ;;
    ft-linux-amd64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-linux-amd64.component-manifest.json"
      ;;
    ft-linux-arm64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-linux-arm64.component-manifest.json"
      ;;
    ft-windows-amd64.zip)
      archive_kind="process-zip"
      manifest_name=""
      ;;
    FrankenTerm-darwin-arm64.app.tar.xz)
      archive_kind="app-tar"
      manifest_name="FrankenTerm.app.component-manifest.json"
      ;;
    *)
      bad "$name has no production archive-inventory contract"
      return
      ;;
  esac

  if python3 - "$ASSETS_DIR/$name" "$archive_kind" "$manifest_name" <<'PY'
import posixpath
import stat
import sys
import tarfile
import zipfile

archive_path, archive_kind, manifest_name = sys.argv[1:]
max_entries = 1_000_000
max_name_bytes = 64 * 1024 * 1024


def canonical_name(name):
    if (not name or name.startswith("/") or posixpath.normpath(name) != name or
            name in (".", "..") or name.startswith("../")):
        raise SystemExit("archive contains a non-canonical member name")
    return name


if archive_kind in ("process-tar", "app-tar"):
    members = []
    name_bytes = 0
    with tarfile.open(archive_path, mode="r:xz") as archive:
        for member in archive:
            if len(members) >= max_entries:
                raise SystemExit("archive exceeds its member-count bound")
            name = canonical_name(member.name)
            name_bytes += len(name.encode("utf-8", "surrogateescape"))
            if name_bytes > max_name_bytes:
                raise SystemExit("archive exceeds its member-name bound")
            if member.islnk() or member.ischr() or member.isblk() or member.isfifo():
                raise SystemExit("archive contains a hard link or special member")
            members.append((name, member))
    names = [name for name, _ in members]
    if len(names) != len(set(names)):
        raise SystemExit("archive contains duplicate member names")
    if archive_kind == "process-tar":
        expected = {
            "ft",
            "frankenterm-mux-server",
            "frankenterm-pty-guardian",
            "verify-components.sh",
            manifest_name,
        }
        if set(names) != expected or any(not member.isfile() for _, member in members):
            raise SystemExit("Unix process-family archive violates the exact five-file installer contract")
    else:
        by_name = dict(members)
        if (set(name.split("/", 1)[0] for name in names) !=
                {"FrankenTerm.app", manifest_name} or
                manifest_name not in by_name or not by_name[manifest_name].isfile() or
                "FrankenTerm.app" not in by_name or not by_name["FrankenTerm.app"].isdir()):
            raise SystemExit("application archive violates its exact top-level installer contract")
        for name, member in members:
            if member.issym():
                target = posixpath.normpath(
                    posixpath.join(posixpath.dirname(name), member.linkname)
                )
                if target != "FrankenTerm.app" and not target.startswith("FrankenTerm.app/"):
                    raise SystemExit("application archive symlink escapes the bundle")
elif archive_kind == "process-zip":
    with zipfile.ZipFile(archive_path) as archive:
        infos = archive.infolist()
        if len(infos) > max_entries:
            raise SystemExit("archive exceeds its member-count bound")
        names = [canonical_name(info.filename) for info in infos]
        if sum(len(name.encode("utf-8", "surrogateescape")) for name in names) > max_name_bytes:
            raise SystemExit("archive exceeds its member-name bound")
        if len(names) != len(set(names)):
            raise SystemExit("archive contains duplicate member names")
        expected = {
            "ft.exe",
            "frankenterm-mux-server.exe",
            "frankenterm-pty-guardian.exe",
        }
        if set(names) != expected or any(info.is_dir() for info in infos):
            raise SystemExit("Windows process-family archive violates its exact executable contract")
        for info in infos:
            mode = info.external_attr >> 16
            if mode and stat.S_ISLNK(mode):
                raise SystemExit("Windows process-family archive contains a symlink")
else:
    raise SystemExit("unknown release archive kind")
PY
  then
    good "$name satisfies its production installer archive-inventory contract"
  else
    bad "$name violates its production installer archive-inventory contract"
  fi
}

resolve_remote_tag_sha() {
  local ref_json object_type object_sha tag_json depth
  ref_json="$(gh api --method GET "repos/$OWNER_REPO/git/ref/tags/$TAG" 2>/dev/null)" || return 1
  object_type="$(jq -r '.object.type // empty' <<< "$ref_json")"
  object_sha="$(jq -r '.object.sha // empty' <<< "$ref_json")"
  depth=0
  while [[ "$object_type" == "tag" && $depth -lt 4 ]]; do
    tag_json="$(gh api --method GET "repos/$OWNER_REPO/git/tags/$object_sha" 2>/dev/null)" || return 1
    object_type="$(jq -r '.object.type // empty' <<< "$tag_json")"
    object_sha="$(jq -r '.object.sha // empty' <<< "$tag_json")"
    depth=$((depth + 1))
  done
  [[ "$object_type" == "commit" && "$object_sha" =~ ^[0-9a-f]{40}$ ]] || return 1
  printf '%s\n' "$object_sha"
}

if [[ -n "$VERSION" ]]; then
  command -v gh >/dev/null 2>&1 || { echo "gh is required for online verification" >&2; exit 2; }
  command -v curl >/dev/null 2>&1 || { echo "curl is required for online verification" >&2; exit 2; }
  if ! curl --help all 2>/dev/null | grep -q -- '--max-filesize'; then
    echo "curl with --max-filesize support is required" >&2
    exit 2
  fi
  if [[ ! "$OWNER_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ || "$OWNER_REPO" == *..* ]]; then
    echo "FT_RELEASE_REPO must be an owner/repository pair" >&2
    exit 2
  fi

  umask 077
  EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ft-verify-release.XXXXXX")"
  ASSETS_DIR="$EVIDENCE_DIR/assets"
  mkdir "$ASSETS_DIR" || { echo "could not create evidence asset directory" >&2; exit 2; }
  note "release verification evidence retained at: $EVIDENCE_DIR"

  if ! gh api --method GET "repos/$OWNER_REPO/releases/tags/$TAG" \
    > "$EVIDENCE_DIR/release.json" 2> "$EVIDENCE_DIR/gh.err"; then
    bad "release $TAG was not found on $OWNER_REPO"
    finish_failed
  fi
  release_metadata_size="$(file_size "$EVIDENCE_DIR/release.json")"
  if [[ ! "$release_metadata_size" =~ ^[0-9]+$ || "$release_metadata_size" -gt 1048576 ]]; then
    bad "release metadata exceeds the 1 MiB verification bound"
    finish_failed
  fi
  if ! jq -e \
    --arg tag "$TAG" \
    --arg sha "$MANIFEST_SHA" '
      .tag_name == $tag and
      .draft == false and
      .prerelease == false and
      .target_commitish == $sha and
      (.assets | type == "array") and
      all(.assets[];
        (.id | type == "number" and floor == . and . > 0) and
        (.name | type == "string") and
        (.size | type == "number" and floor == . and . > 0) and
        .state == "uploaded" and
        (.digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) and
        (.browser_download_url | type == "string")
      ) and
      ([.assets[].id] | unique | length) == (.assets | length) and
      ([.assets[].name] | unique | length) == (.assets | length)
    ' "$EVIDENCE_DIR/release.json" >/dev/null 2>&1; then
    bad "release metadata does not bind $TAG to manifest source $MANIFEST_SHA"
  fi

  expected_names="$(printf '%s\n' "${EXPECTED_ASSETS[@]}" | LC_ALL=C sort)"
  if ! remote_names="$(jq -r '.assets[].name' "$EVIDENCE_DIR/release.json" | LC_ALL=C sort)"; then
    bad "release asset names could not be read"
    finish_failed
  fi
  if [[ "$remote_names" != "$expected_names" ]]; then
    bad "remote release assets are not the exact 17-file strict DSR plan"
  else
    good "remote release exposes the exact 17-file strict DSR plan with no manifest asset"
  fi

  if ! remote_tag_sha="$(resolve_remote_tag_sha)"; then
    bad "remote tag $TAG could not be resolved to one commit"
  elif [[ "$remote_tag_sha" != "$MANIFEST_SHA" ]]; then
    bad "remote tag resolves to $remote_tag_sha, not manifest source $MANIFEST_SHA"
  else
    good "remote tag resolves to manifest source $MANIFEST_SHA"
  fi
  [[ $fail -eq 0 ]] || finish_failed

  for name in "${EXPECTED_ASSETS[@]}"; do
    declared_size="$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .size' "$EVIDENCE_DIR/release.json")"
    declared_digest="$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .digest' "$EVIDENCE_DIR/release.json")"
    download_url="$(jq -r --arg name "$name" '.assets[] | select(.name == $name) | .browser_download_url' "$EVIDENCE_DIR/release.json")"
    max_size="$(max_size_for_asset "$name")" || {
      bad "no download bound is defined for $name"
      continue
    }
    expected_url="https://github.com/$OWNER_REPO/releases/download/$TAG/$name"
    if [[ ! "$declared_size" =~ ^[0-9]+$ || "$declared_size" -le 0 || "$declared_size" -gt "$max_size" ]]; then
      bad "$name declared size is absent or exceeds its $max_size-byte bound"
      continue
    fi
    if [[ "$download_url" != "$expected_url" ]]; then
      bad "$name has an unexpected download URL"
      continue
    fi
    if ! curl --fail --location --silent --show-error \
      --connect-timeout 20 --max-time 600 --max-filesize "$max_size" \
      --proto '=https' --proto-redir '=https' \
      --output "$ASSETS_DIR/$name" "$download_url"; then
      bad "$name could not be downloaded within its size/time bounds"
      continue
    fi
    actual_size="$(file_size "$ASSETS_DIR/$name")"
    if [[ "$actual_size" != "$declared_size" ]]; then
      bad "$name downloaded size $actual_size does not match declared size $declared_size"
    elif [[ "sha256:$(sha256_file "$ASSETS_DIR/$name")" != "$declared_digest" ]]; then
      bad "$name downloaded bytes do not match the GitHub asset digest"
    else
      good "$name downloaded within its bound and matches its GitHub digest ($actual_size bytes)"
    fi
  done
  [[ $fail -eq 0 ]] || finish_failed
fi

verify_exact_inventory "$ASSETS_DIR"
[[ $fail -eq 0 ]] || finish_failed
ASSETS_DIR="$(cd "$ASSETS_DIR" && pwd -P)"

for name in "${EXPECTED_ASSETS[@]}"; do
  max_size="$(max_size_for_asset "$name")" || {
    bad "no file-size bound is defined for $name"
    continue
  }
  actual_size="$(file_size "$ASSETS_DIR/$name")"
  if [[ ! "$actual_size" =~ ^[0-9]+$ || "$actual_size" -le 0 || "$actual_size" -gt "$max_size" ]]; then
    bad "$name is empty or exceeds its $max_size-byte bound"
  fi
done
[[ $fail -eq 0 ]] || finish_failed

for name in "${BASE_ASSETS[@]}"; do
  expected_sha="$(jq -r --arg name "$name" '.artifacts[] | select(.name == $name) | .sha256' "$MANIFEST")"
  expected_size="$(jq -r --arg name "$name" '.artifacts[] | select(.name == $name) | .size_bytes' "$MANIFEST")"
  actual_sha="$(sha256_file "$ASSETS_DIR/$name")"
  actual_size="$(file_size "$ASSETS_DIR/$name")"
  if [[ "$actual_sha" != "$expected_sha" || "$actual_size" != "$expected_size" ]]; then
    bad "$name does not match its build-manifest hash and size"
  else
    good "$name matches its build-manifest hash and size"
  fi

  if ! cmp -s "$ASSETS_DIR/$name.sha256" <(printf '%s  %s\n' "$expected_sha" "$name"); then
    bad "$name.sha256 is not the exact manifest-derived checksum sidecar"
  else
    good "$name.sha256 exactly binds $name"
  fi

  if ! minisign -V -H -q -p "$PUBKEY" -m "$ASSETS_DIR/$name" \
    -x "$ASSETS_DIR/$name.minisig" >/dev/null 2>&1; then
    bad "$name.minisig does not verify against release/minisign.pub"
  else
    good "$name.minisig verifies against release/minisign.pub"
  fi

  verify_archive_inventory "$name"
done

if ! cmp -s "$ASSETS_DIR/SHA256SUMS" <(
  for name in "${BASE_ASSETS[@]}"; do
    jq -r --arg name "$name" '.artifacts[] | select(.name == $name) | "\(.sha256)  \(.name)"' "$MANIFEST"
  done | LC_ALL=C sort
); then
  bad "SHA256SUMS is not the exact sorted manifest-derived aggregate"
else
  good "SHA256SUMS exactly binds all five manifest artifacts"
fi

if ! minisign -V -H -q -p "$PUBKEY" -m "$ASSETS_DIR/SHA256SUMS" \
  -x "$ASSETS_DIR/SHA256SUMS.minisig" >/dev/null 2>&1; then
  bad "SHA256SUMS.minisig does not verify against release/minisign.pub"
else
  good "SHA256SUMS.minisig verifies against release/minisign.pub"
fi

if [[ -n "$VERSION" && $fail -eq 0 ]]; then
  if ! gh api --method GET "repos/$OWNER_REPO/releases/tags/$TAG" \
    > "$EVIDENCE_DIR/release-after.json" 2> "$EVIDENCE_DIR/gh-after.err"; then
    bad "release $TAG could not be re-read after byte verification"
  else
    release_metadata_size="$(file_size "$EVIDENCE_DIR/release-after.json")"
    if [[ ! "$release_metadata_size" =~ ^[0-9]+$ || "$release_metadata_size" -gt 1048576 ]]; then
      bad "post-verification release metadata exceeds the 1 MiB bound"
    elif ! jq -en \
      --slurpfile before "$EVIDENCE_DIR/release.json" \
      --slurpfile after "$EVIDENCE_DIR/release-after.json" '
        def identity:
          {
            id,
            tag_name,
            target_commitish,
            draft,
            prerelease,
            assets: ([.assets[] |
              {id, name, size, state, digest, browser_download_url}] | sort_by(.id))
          };
        ($before[0] | identity) == ($after[0] | identity)
      ' >/dev/null 2>&1; then
      bad "release identity or asset set changed during verification"
    fi
  fi

  if ! remote_tag_sha="$(resolve_remote_tag_sha)"; then
    bad "remote tag $TAG could not be re-resolved after byte verification"
  elif [[ "$remote_tag_sha" != "$MANIFEST_SHA" ]]; then
    bad "remote tag changed during verification"
  else
    good "release asset identity and remote tag stayed stable during verification"
  fi
fi

if [[ $fail -gt 0 ]]; then
  finish_failed
fi
if [[ "$(sha256_file "$MANIFEST")" != "$MANIFEST_FILE_SHA" || \
      "$(file_size "$MANIFEST")" != "$manifest_size" ]]; then
  bad "local DSR build manifest changed during verification"
  finish_failed
fi
if [[ -n "$ATTESTATION_BUNDLE" ]]; then
  echo "release verification: exact 17-asset strict DSR set and externally authenticated attestation passed for $TAG"
  echo "scope: artifact integrity and attestation policy bindings; target execution and installer/canary outcomes require their own evidence"
else
  echo "release verification: artifact-set checks passed (exact 17-asset strict DSR set for $TAG); attestation not requested"
fi
exit 0
