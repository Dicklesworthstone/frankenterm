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
#   scripts/release/verify-release.sh [--channel stable|companion-appstore] [--json] <version> [--manifest <path>]
#       Verify a published release. Without --manifest (stable channel), use DSR's
#       standard local build-manifest path for the version.
#       With --channel companion-appstore, verify a macOS App Store / Exporter
#       companion release (`vX.Y.Z-appstore.N`), requiring prerelease=true (never
#       hijacking GitHub Latest), and downloading and authenticating the Exporter
#       DMG, optional app ZIP, checksum sidecars, and signed SHA256SUMS.
#       Online checks also pin and recheck the release, assets, and resolved tag.
#
#   scripts/release/verify-release.sh [--channel stable] [--json] --manifest <path> --assets-dir <dir>
#       Offline verification of the same closed asset set. The local manifest
#       may be in <dir>, but is not counted as a release asset.
#
#   scripts/release/verify-release.sh --channel companion-appstore [--json] <version> --release-json <path> [--assets-dir <dir>]
#       Offline verification requires both captured GitHub release JSON and
#       downloaded assets. Metadata alone never establishes artifact integrity.
#       Native notarization/stapling and install execution are separate checks.
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
#   5. installer archive inventories and direct Linux ELF ABI requirements;
#   6. online release/tag identity bound to the manifest source commit.
#
# This preserves the v0.15.1 negative contract: its four-artifact unsigned
# build output cannot satisfy the current strict five-artifact release plan.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
PUBKEY="$ROOT/release/minisign.pub"
OWNER_REPO="${FT_RELEASE_REPO:-Dicklesworthstone/frankenterm}"
DSR_STATE_ROOT="${DSR_STATE_DIR:-${XDG_STATE_HOME:-${HOME:?HOME is required}/.local/state}/dsr}"

CHANNEL="stable"
JSON_OUTPUT=0
VERSION=""
MANIFEST=""
ASSETS_DIR=""
RELEASE_JSON_FIXTURE=""
ATTESTATION_BUNDLE=""
RELEASE_POLICY="${FT_ATTESTATION_RELEASE_POLICY:-}"
VERIFICATION_SCOPE="artifact_set"

usage() {
  sed -n '2,42p' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --channel)
      [[ $# -ge 2 ]] || { echo "--channel needs stable or companion-appstore" >&2; exit 2; }
      CHANNEL="$2"
      shift 2
      ;;
    --json)
      JSON_OUTPUT=1
      shift
      ;;
    --release-json)
      [[ $# -ge 2 ]] || { echo "--release-json needs a path" >&2; exit 2; }
      RELEASE_JSON_FIXTURE="$2"
      shift 2
      ;;
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

case "$CHANNEL" in
  stable|companion-appstore) ;;
  *)
    echo "unknown --channel '$CHANNEL' (expected: stable or companion-appstore)" >&2
    exit 2
    ;;
esac

if [[ -n "$ATTESTATION_BUNDLE" && -z "$RELEASE_POLICY" || -z "$ATTESTATION_BUNDLE" && -n "$RELEASE_POLICY" ]]; then
  echo "--attestation-bundle and --release-policy (or FT_ATTESTATION_RELEASE_POLICY) are required together" >&2
  exit 2
fi
if [[ "$CHANNEL" == "stable" && -n "$VERSION" && -n "$ASSETS_DIR" ]]; then
  echo "--assets-dir is only valid for offline --manifest verification on --channel stable" >&2
  exit 2
fi
if [[ "$CHANNEL" == "stable" && -z "$VERSION" && ( -z "$MANIFEST" || -z "$ASSETS_DIR" ) ]]; then
  echo "usage: $0 [--channel stable] <version> [--manifest <path>] | --manifest <path> --assets-dir <dir>" >&2
  exit 2
fi
if [[ "$CHANNEL" == "companion-appstore" && -z "$VERSION" ]]; then
  echo "usage: $0 --channel companion-appstore <vX.Y.Z-appstore.N> [--release-json <path>] [--assets-dir <dir>]" >&2
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
FAIL_REASONS=()
FAIL_CODE=""
note() { [[ "$JSON_OUTPUT" -eq 1 ]] || printf '%s\n' "$*"; }
bad() {
  FAIL_REASONS+=("$*")
  [[ "$JSON_OUTPUT" -eq 1 ]] || printf 'FAIL %s\n' "$*"
  fail=$((fail + 1))
}
good() { [[ "$JSON_OUTPUT" -eq 1 ]] || printf 'ok   %s\n' "$*"; }

emit_json_verdict() {
  local ok_flag="$1"
  local tag_val="${TAG:-$VERSION}"
  local sha_val="${MANIFEST_SHA:-}"
  local code_val="$FAIL_CODE"
  local reasons_json
  if [[ ${#FAIL_REASONS[@]} -eq 0 ]]; then
    reasons_json="[]"
  else
    reasons_json="$(printf '%s\n' "${FAIL_REASONS[@]}" | jq -R . | jq -s .)"
  fi
  jq -n \
    --arg schema "frankenterm.release_verification.v1" \
    --arg channel "$CHANNEL" \
    --arg verification_scope "$VERIFICATION_SCOPE" \
    --arg tag "$tag_val" \
    --arg commit "$sha_val" \
    --arg fail_code "$code_val" \
    --argjson ok "$ok_flag" \
    --argjson failure_count "$fail" \
    --argjson failures "$reasons_json" \
    '{
      schema_version: $schema,
      ok: $ok,
      channel: $channel,
      verification_scope: $verification_scope,
      tag: $tag,
      commit: (if $commit == "" then null else $commit end),
      fail_code: (if $fail_code == "" then null else $fail_code end),
      failure_count: $failure_count,
      failures: $failures
    }'
}

finish_failed() {
  if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    emit_json_verdict false
  else
    echo "release verification ($CHANNEL): $fail failure(s) — do not publish or announce"
  fi
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
    *.tar.xz|*.zip|*.dmg) printf '%s\n' 1073741824 ;;
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

release_metadata_is_bounded_json() {
  local path="$1" size
  regular_file "$path" || return 1
  size="$(file_size "$path")"
  [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le 1048576 ]] || return 1
  python3 - "$path" <<'PY'
import json
import sys

def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result

try:
    with open(sys.argv[1], "rb") as source:
        json.load(source, object_pairs_hook=unique_object)
except (OSError, ValueError) as error:
    sys.stderr.write(f"invalid release metadata: {error}\n")
    sys.exit(1)
PY
}

release_identity_is_unchanged() {
  jq -en --slurpfile before "$1" --slurpfile after "$2" '
    def identity:
      {
        id,
        tag_name,
        target_commitish,
        draft,
        prerelease,
        assets: ([.assets[] |
          {id, name, size, state, digest, browser_download_url}] | sort_by(.id, .name))
      };
    ($before[0] | identity) == ($after[0] | identity)
  ' >/dev/null 2>&1
}

parse_remote_tag_object() {
  python3 -c '
import json
import re
import sys

def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate tag JSON key")
        result[key] = value
    return result

try:
    payload = sys.stdin.buffer.read(1048577)
    if not 0 < len(payload) <= 1048576:
        raise ValueError("tag response exceeds its byte bound")
    response = json.loads(payload, object_pairs_hook=unique_object)
    obj = response.get("object") if isinstance(response, dict) else None
    if (not isinstance(obj, dict) or obj.get("type") not in ("commit", "tag") or
            not isinstance(obj.get("sha"), str) or
            re.fullmatch(r"[0-9a-f]{40}", obj["sha"]) is None):
        raise ValueError("invalid tag object identity")
except (OSError, TypeError, ValueError, RecursionError):
    sys.exit(1)
print(obj["type"] + "\t" + obj["sha"])
'
}

resolve_remote_tag_sha() {
  local ref_json object_type object_sha object_identity tag_json depth
  ref_json="$(gh api --method GET "repos/$OWNER_REPO/git/ref/tags/$TAG" 2>/dev/null)" || return 1
  object_identity="$(parse_remote_tag_object <<< "$ref_json")" || return 1
  IFS=$'\t' read -r object_type object_sha <<< "$object_identity"
  depth=0
  while [[ "$object_type" == "tag" && $depth -lt 4 ]]; do
    tag_json="$(gh api --method GET "repos/$OWNER_REPO/git/tags/$object_sha" 2>/dev/null)" || return 1
    object_identity="$(parse_remote_tag_object <<< "$tag_json")" || return 1
    IFS=$'\t' read -r object_type object_sha <<< "$object_identity"
    depth=$((depth + 1))
  done
  [[ "$object_type" == "commit" && "$object_sha" =~ ^[0-9a-f]{40}$ ]] || return 1
  printf '%s\n' "$object_sha"
}

verify_companion_appstore_channel() {
  VERIFICATION_SCOPE="companion_artifact_integrity"
  if [[ ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+-appstore\.[0-9]+$ ]]; then
    FAIL_CODE="channel_mismatch"
    bad "channel_mismatch: companion-appstore requires vX.Y.Z-appstore.N tag (got: $VERSION)"
    finish_failed
  fi
  TAG="v${VERSION#v}"
  if [[ -n "$MANIFEST" || -n "$ATTESTATION_BUNDLE" || -n "$RELEASE_POLICY" ]]; then
    FAIL_CODE="unsupported_companion_option"
    bad "companion-appstore does not implement DSR manifest or attestation verification; requested checks cannot be ignored"
    finish_failed
  fi
  if [[ -n "$RELEASE_JSON_FIXTURE" && -z "$ASSETS_DIR" ]]; then
    FAIL_CODE="missing_companion_assets"
    bad "offline companion verification requires --assets-dir; release metadata alone cannot authenticate downloads"
    finish_failed
  fi
  local rel_json=""
  local evidence_dir=""
  local initial_tag_sha="" final_tag_sha=""
  if [[ -n "$RELEASE_JSON_FIXTURE" ]]; then
    if ! regular_file "$RELEASE_JSON_FIXTURE"; then
      FAIL_CODE="missing_release_json"
      bad "companion release metadata file missing or not regular: $RELEASE_JSON_FIXTURE"
      finish_failed
    fi
    rel_json="$RELEASE_JSON_FIXTURE"
  else
    command -v gh >/dev/null 2>&1 || { echo "gh is required for online companion verification" >&2; exit 2; }
    if [[ ! "$OWNER_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ || "$OWNER_REPO" == *..* ]]; then
      echo "FT_RELEASE_REPO must be an owner/repository pair" >&2
      exit 2
    fi
    umask 077
    evidence_dir="$(mktemp -d "${TMPDIR:-/tmp}/ft-verify-companion.XXXXXX")" || exit 2
    rel_json="$evidence_dir/release.json"
    note "companion verification evidence retained at: $evidence_dir"
    if ! gh api --method GET "repos/$OWNER_REPO/releases/tags/$TAG" > "$rel_json" 2> "$evidence_dir/gh.err"; then
      FAIL_CODE="release_not_found"
      bad "companion release $TAG was not found on $OWNER_REPO"
      finish_failed
    fi
  fi

  if ! release_metadata_is_bounded_json "$rel_json"; then
    FAIL_CODE="invalid_companion_metadata"
    bad "companion release metadata is not bounded, duplicate-free JSON"
    finish_failed
  fi
  if ! jq -e --arg tag "$TAG" '
    .tag_name == $tag and .draft == false and
    (.assets | type == "array") and
    all(.assets[];
      (.name | type == "string") and
      (.size | type == "number" and floor == . and . > 0 and . <= 4294967296)) and
    ([.assets[].name] | unique | length) == (.assets | length)
  ' "$rel_json" >/dev/null 2>&1; then
    FAIL_CODE="invalid_companion_metadata"
    bad "companion release metadata has invalid or duplicate assets, is draft, or tag_name != $TAG"
    finish_failed
  fi
  if [[ -z "$RELEASE_JSON_FIXTURE" ]] && ! jq -e '
    def valid_id:
      type == "number" and floor == . and . > 0 and . <= 9007199254740991;
    (.id | valid_id) and (.target_commitish | type == "string") and
    all(.assets[];
      (.id | valid_id) and .state == "uploaded" and
      (.browser_download_url | type == "string") and
      (.digest == null or (.digest | type == "string" and test("^sha256:[0-9a-f]{64}$")))) and
    ([.assets[].id] | unique | length) == (.assets | length)
  ' "$rel_json" >/dev/null 2>&1; then
    FAIL_CODE="invalid_companion_metadata"
    bad "online companion metadata lacks distinct uploaded release/asset identities"
    finish_failed
  fi
  MANIFEST_SHA="$(jq -r '.target_commitish // "" | select(type == "string" and test("^[0-9a-f]{40}$"))' "$rel_json")"
  if ! jq -e '.prerelease == true' "$rel_json" >/dev/null 2>&1; then
    FAIL_CODE="companion_release_not_prerelease"
    bad "companion_release_not_prerelease: $TAG has prerelease=false and can hijack cross-platform GitHub Latest (remediate: gh release edit $TAG --prerelease)"
  else
    good "$TAG is marked prerelease=true (isolated from cross-platform GitHub Latest)"
  fi

  local companion_artifacts=("FrankenTerm-Exporter.dmg")
  if jq -e '.assets[] | select(.name == "FrankenTerm-Exporter.app.zip")' "$rel_json" >/dev/null; then
    companion_artifacts+=("FrankenTerm-Exporter.app.zip")
  fi
  local required_companion=("SHA256SUMS" "SHA256SUMS.minisig")
  local req artifact
  for artifact in "${companion_artifacts[@]}"; do
    required_companion+=("$artifact" "$artifact.sha256" "$artifact.minisig")
  done
  for req in "${required_companion[@]}"; do
    if ! jq -e --arg n "$req" '.assets[] | select(.name == $n and (.size | type == "number" and . > 0))' "$rel_json" >/dev/null 2>&1; then
      FAIL_CODE="${FAIL_CODE:-missing_companion_asset}"
      bad "missing required companion-appstore asset: $req"
    else
      good "companion metadata includes $req"
    fi
  done
  [[ $fail -eq 0 ]] || finish_failed

  if [[ -z "$RELEASE_JSON_FIXTURE" ]]; then
    # A branch name in target_commitish is not source identity. Resolve the
    # actual tag before any lengthy downloads, then pin that commit through
    # the final release snapshot check below.
    if ! initial_tag_sha="$(resolve_remote_tag_sha)"; then
      FAIL_CODE="companion_tag_unavailable"
      bad "companion tag $TAG could not be resolved to one commit"
      finish_failed
    fi
    MANIFEST_SHA="$initial_tag_sha"
    good "companion tag $TAG resolves to $initial_tag_sha"
  fi

  if [[ -z "$ASSETS_DIR" ]]; then
    command -v curl >/dev/null 2>&1 || { echo "curl is required for online companion verification" >&2; exit 2; }
    ASSETS_DIR="$evidence_dir/assets"
    mkdir "$ASSETS_DIR" || exit 2
    local download_url expected_url declared_size max_size
    for req in "${required_companion[@]}"; do
      max_size="$(max_size_for_asset "$req")" || exit 2
      declared_size="$(jq -r --arg n "$req" '.assets[] | select(.name == $n) | .size' "$rel_json")"
      download_url="$(jq -r --arg n "$req" '.assets[] | select(.name == $n) | .browser_download_url // ""' "$rel_json")"
      expected_url="https://github.com/$OWNER_REPO/releases/download/$TAG/$req"
      if [[ "$declared_size" -gt "$max_size" || "$download_url" != "$expected_url" ]]; then
        FAIL_CODE="${FAIL_CODE:-invalid_companion_asset}"
        bad "companion asset $req exceeds its size bound or has an unexpected download URL"
        continue
      fi
      if ! curl --fail --location --silent --show-error \
          --user-agent 'frankenterm-agent (+https://github.com/Dicklesworthstone/frankenterm)' \
          --connect-timeout 20 --max-time 600 --max-filesize "$max_size" \
          --proto '=https' --proto-redir '=https' \
          --output "$ASSETS_DIR/$req" "$download_url"; then
        FAIL_CODE="${FAIL_CODE:-companion_download_failed}"
        bad "companion asset $req could not be downloaded within its size/time bounds"
      fi
    done
    [[ $fail -eq 0 ]] || finish_failed
  fi

  if [[ ! -d "$ASSETS_DIR" || -L "$ASSETS_DIR" ]]; then
    FAIL_CODE="invalid_assets_dir"
    bad "companion assets directory missing or symlink: $ASSETS_DIR"
    finish_failed
  fi
  local actual_size expected_size asset_sha asset_digest bound
  for req in "${required_companion[@]}"; do
    if ! regular_file "$ASSETS_DIR/$req"; then
      FAIL_CODE="${FAIL_CODE:-missing_local_companion_asset}"
      bad "missing or non-regular local companion asset: $req"
      continue
    fi
    bound="$(max_size_for_asset "$req")" || exit 2
    actual_size="$(file_size "$ASSETS_DIR/$req")"
    expected_size="$(jq -r --arg n "$req" '.assets[] | select(.name == $n) | .size' "$rel_json")"
    if [[ "$actual_size" -le 0 || "$actual_size" -gt "$bound" || "$actual_size" != "$expected_size" ]]; then
      FAIL_CODE="${FAIL_CODE:-companion_size_mismatch}"
      bad "companion asset $req is empty, oversized, or does not match its metadata size"
    fi
    asset_digest="$(jq -r --arg n "$req" '.assets[] | select(.name == $n) | .digest // ""' "$rel_json")"
    if [[ -n "$asset_digest" && "$asset_digest" != "sha256:$(sha256_file "$ASSETS_DIR/$req")" ]]; then
      FAIL_CODE="${FAIL_CODE:-companion_digest_mismatch}"
      bad "companion asset $req does not match its metadata digest"
    fi
  done
  [[ $fail -eq 0 ]] || finish_failed

  for artifact in "${companion_artifacts[@]}"; do
    asset_sha="$(sha256_file "$ASSETS_DIR/$artifact")"
    if ! cmp -s "$ASSETS_DIR/$artifact.sha256" <(printf '%s  %s\n' "$asset_sha" "$artifact"); then
      FAIL_CODE="${FAIL_CODE:-checksum_mismatch}"
      bad "$artifact.sha256 is not the canonical checksum sidecar for the verified bytes"
    else
      good "$artifact.sha256 binds the artifact digest"
    fi
  done
  for artifact in "${companion_artifacts[@]}" SHA256SUMS; do
    if ! minisign -V -H -q -p "$PUBKEY" -m "$ASSETS_DIR/$artifact" -x "$ASSETS_DIR/$artifact.minisig" >/dev/null 2>&1; then
      FAIL_CODE="${FAIL_CODE:-minisig_invalid}"
      bad "$artifact.minisig failed verification against release/minisign.pub"
    else
      good "$artifact.minisig verified against release/minisign.pub"
    fi
  done
  if ! python3 - "$ASSETS_DIR" "${companion_artifacts[@]}" <<'PY'
import hashlib
import re
import sys
from pathlib import Path

directory = Path(sys.argv[1])
entries = {}
for line in (directory / "SHA256SUMS").read_bytes().splitlines(keepends=True):
    match = re.fullmatch(rb"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)\n", line)
    if match is None:
        raise SystemExit("SHA256SUMS contains a malformed or unsafe entry")
    digest, name = (value.decode("ascii") for value in match.groups())
    if name in entries:
        raise SystemExit("SHA256SUMS contains duplicate artifact entries")
    entries[name] = digest
for name in sys.argv[2:]:
    hasher = hashlib.sha256()
    with (directory / name).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    if entries.get(name) != hasher.hexdigest():
        raise SystemExit(f"SHA256SUMS does not bind the verified bytes of {name}")
PY
  then
    FAIL_CODE="${FAIL_CODE:-aggregate_checksum_mismatch}"
    bad "signed SHA256SUMS does not bind every verified companion artifact"
  fi

  if [[ $fail -gt 0 ]]; then
    finish_failed
  fi
  if [[ -z "$RELEASE_JSON_FIXTURE" ]]; then
    # Signatures authenticate the downloaded bytes, not a mutable GitHub
    # channel, asset inventory, or tag. A publication edit during verification
    # must not turn an isolated prerelease into a successful Latest verdict.
    if ! gh api --method GET "repos/$OWNER_REPO/releases/tags/$TAG" \
        > "$evidence_dir/release-after.json" 2> "$evidence_dir/gh-after.err"; then
      FAIL_CODE="companion_release_unavailable"
      bad "companion release $TAG could not be re-read after byte verification"
    elif ! release_metadata_is_bounded_json "$evidence_dir/release-after.json"; then
      FAIL_CODE="invalid_companion_metadata"
      bad "post-verification companion metadata is not bounded, duplicate-free JSON"
    elif ! release_identity_is_unchanged "$rel_json" "$evidence_dir/release-after.json"; then
      FAIL_CODE="companion_release_changed"
      bad "companion release identity, channel, or asset set changed during verification"
    fi
    if ! final_tag_sha="$(resolve_remote_tag_sha)"; then
      FAIL_CODE="${FAIL_CODE:-companion_tag_unavailable}"
      bad "companion tag $TAG could not be re-resolved after byte verification"
    elif [[ "$final_tag_sha" != "$initial_tag_sha" ]]; then
      FAIL_CODE="${FAIL_CODE:-companion_tag_changed}"
      bad "companion tag changed during verification"
    fi
    [[ $fail -eq 0 ]] || finish_failed
    good "companion release identity, prerelease channel, assets, and tag stayed stable during verification"
  fi
  if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    emit_json_verdict true
  else
    echo "release verification (companion-appstore): artifact integrity checks passed for $TAG"
    echo "scope: signed companion artifacts; native notarization/stapling and install execution require separate evidence"
  fi
  exit 0
}

if [[ "$CHANNEL" == "companion-appstore" ]]; then
  verify_companion_appstore_channel
fi

TAG=""
if [[ -n "$VERSION" ]]; then
  if [[ ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    FAIL_CODE="channel_mismatch"
    bad "channel_mismatch: --channel stable requires vX.Y.Z semver (got: $VERSION; use --channel companion-appstore for -appstore.N releases)"
    finish_failed
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
  VERIFICATION_SCOPE="artifact_set_and_attestation"
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
  local archive_kind manifest_name linux_target="" archive_error
  case "$name" in
    ft-darwin-arm64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-darwin-arm64.component-manifest.json"
      ;;
    ft-linux-amd64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-linux-amd64.component-manifest.json"
      linux_target="x86_64-unknown-linux-gnu"
      ;;
    ft-linux-arm64.tar.xz)
      archive_kind="process-tar"
      manifest_name="ft-linux-arm64.component-manifest.json"
      linux_target="aarch64-unknown-linux-gnu"
      ;;
    ft-windows-amd64.zip)
      archive_kind="process-zip"
      manifest_name="ft-windows-amd64.component-manifest.json"
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

  if archive_error=$(python3 - "$ASSETS_DIR/$name" "$archive_kind" "$manifest_name" \
      "$linux_target" "$ROOT/scripts/check-linux-release-abi.py" 2>&1 <<'PY'
from contextlib import ExitStack
import json
from pathlib import Path
import posixpath
import shutil
import subprocess
import tempfile
import stat
import sys
import tarfile
import zipfile

archive_path, archive_kind, manifest_name, linux_target, abi_checker = sys.argv[1:]
executables = {"ft", "frankenterm-mux-server", "frankenterm-pty-guardian"}
process_members = executables | {"verify-components.sh", manifest_name}
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
    with ExitStack() as resources:
        archive = resources.enter_context(tarfile.open(archive_path, mode="r|xz" if linux_target else "r:xz"))
        if linux_target:
            staging = Path(resources.enter_context(tempfile.TemporaryDirectory(prefix="ft-release-abi-")))
            seen = set()
        for member in archive:
            if len(members) >= max_entries:
                raise SystemExit("archive exceeds its member-count bound")
            name = canonical_name(member.name)
            name_bytes += len(name.encode("utf-8", "surrogateescape"))
            if name_bytes > max_name_bytes:
                raise SystemExit("archive exceeds its member-name bound")
            if member.islnk() or member.ischr() or member.isblk() or member.isfifo():
                raise SystemExit("archive contains a hard link or special member")
            if linux_target:
                if name not in process_members or name in seen or not member.isfile():
                    raise SystemExit("invalid or duplicate Linux process-family archive member")
                if not 0 <= member.size <= 1024 * 1024 * 1024:
                    raise SystemExit(f"{name} exceeds the 1 GiB expanded member bound")
                seen.add(name)
                if name in executables:
                    # Only fixed component names reach disk. Never execute a release binary.
                    with archive.extractfile(member) as source, (staging / name).open("xb") as dest:
                        shutil.copyfileobj(source, dest, 1024 * 1024)
            members.append((name, member))
        if linux_target:
            if seen != process_members:
                raise SystemExit("Linux process-family archive is missing required members")
            result = subprocess.run(
                [sys.executable, abi_checker, "--target", linux_target,
                 "--ft", staging / "ft", "--mux", staging / "frankenterm-mux-server",
                 "--guardian", staging / "frankenterm-pty-guardian"],
                capture_output=True, text=True,
            )
            if result.returncode:
                if not result.stdout.strip():
                    raise SystemExit("Linux ABI checker did not report a verdict: " + result.stderr.strip())
                report = json.loads(result.stdout)
                reasons = [f"{key}: {component['reason']}" for key, component in
                           report["components"].items() if component["status"] == "failed"]
                raise SystemExit("Linux ABI verification failed: " +
                                 (report.get("reason") or ", ".join(reasons)))
    names = [name for name, _ in members]
    if len(names) != len(set(names)):
        raise SystemExit("archive contains duplicate member names")
    if archive_kind == "process-tar":
        if set(names) != process_members or any(not member.isfile() for _, member in members):
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
            "frankenterm-gui.exe",
            "verify-components.sh",
            manifest_name,
        }
        if set(names) != expected or any(info.is_dir() for info in infos):
            raise SystemExit("Windows application-family archive violates its exact six-file contract")
        for info in infos:
            mode = info.external_attr >> 16
            if stat.S_IFMT(mode) not in (0, stat.S_IFREG):
                raise SystemExit("Windows application-family archive contains a non-regular member")
else:
    raise SystemExit("unknown release archive kind")
PY
  ); then
    good "$name satisfies its production installer archive-inventory contract"
    if [[ -n "$linux_target" ]]; then
      good "$name: all three executables satisfy the $linux_target ABI baseline"
    fi
  else
    bad "$name failed archive verification: $archive_error"
  fi
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

  # Inspect only authenticated bytes. No platform binary is executed.
  if [[ $fail -eq 0 ]]; then
    verify_archive_inventory "$name"
  fi
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
    elif ! release_identity_is_unchanged "$EVIDENCE_DIR/release.json" "$EVIDENCE_DIR/release-after.json"; then
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
if [[ "$JSON_OUTPUT" -eq 1 ]]; then
  emit_json_verdict true
elif [[ -n "$ATTESTATION_BUNDLE" ]]; then
  echo "release verification: exact 17-asset strict DSR set and externally authenticated attestation passed for $TAG"
  echo "scope: artifact integrity and attestation policy bindings; target execution and installer/canary outcomes require their own evidence"
else
  echo "release verification: artifact-set checks passed (exact 17-asset strict DSR set for $TAG); attestation not requested"
fi
exit 0
