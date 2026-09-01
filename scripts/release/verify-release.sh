#!/usr/bin/env bash
# scripts/release/verify-release.sh — refuse a release whose artifacts are not
# signed the way install.sh requires.
#
# Why this exists: v0.15.1 (2026-08-21) was published with every artifact
# recorded as `"signed": false` and no `.minisig` assets, while install.sh
# exits 1 when minisign verification fails unless the user passes
# `--no-verify`. The documented one-line install could not install the latest
# release. This check makes that state impossible to publish silently again.
# It is part of the DSR release closeout (README "Quick Install",
# docs/release/attestation-checklist.md); see ft-xxfwy.1 / ft-xxfwy.2.
#
# Usage:
#   scripts/release/verify-release.sh <version>            # e.g. v0.15.2 or 0.15.2
#   scripts/release/verify-release.sh --manifest <path> [--assets-dir <dir>]
#                                                          # offline: verify a local DSR manifest
#                                                          # (and .minisig files next to the assets)
#
# Checks, all fail-closed:
#   1. the DSR manifest exists and reports status "success" with no failed targets;
#   2. every artifact entry has "signed": true and a non-empty "signature_file";
#   3. every artifact has a published `<asset>.minisig` (GitHub release asset, or
#      a file in --assets-dir), and `SHA256SUMS` / `SHA256SUMS.minisig` exist;
#   4. every signature verifies against release/minisign.pub when the asset is
#      available locally (offline mode) — the same key install.sh pins.
# Exit 0 only when every check passes.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PUBKEY="$ROOT/release/minisign.pub"
OWNER_REPO="${FT_RELEASE_REPO:-Dicklesworthstone/frankenterm}"

VERSION=""
MANIFEST=""
ASSETS_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest) shift; MANIFEST="${1:?--manifest needs a path}" ;;
    --assets-dir) shift; ASSETS_DIR="${1:?--assets-dir needs a directory}" ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) VERSION="$1" ;;
  esac
  shift
done
if [[ -z "$VERSION" && -z "$MANIFEST" ]]; then
  echo "usage: $0 <version> | --manifest <path> [--assets-dir <dir>]" >&2
  exit 2
fi
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }

fail=0
note() { printf '%s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; fail=$((fail + 1)); }
good() { printf 'ok   %s\n' "$*"; }

tmp="$(mktemp -d "${TMPDIR:-/tmp}/ft-verify-release.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

declare -a published=()
if [[ -n "$VERSION" ]]; then
  tag="v${VERSION#v}"
  command -v gh >/dev/null 2>&1 || { echo "gh is required for online verification" >&2; exit 2; }
  if ! gh release view "$tag" -R "$OWNER_REPO" --json assets -q '.assets[].name' > "$tmp/assets.txt" 2>"$tmp/gh.err"; then
    bad "release $tag not found on $OWNER_REPO: $(tr -d '\n' < "$tmp/gh.err")"
    echo "release verification: $fail failure(s)"; exit 1
  fi
  mapfile -t published < "$tmp/assets.txt"
  manifest_name="frankenterm-${tag}-manifest.json"
  if printf '%s\n' "${published[@]}" | grep -qx "$manifest_name"; then
    if gh release download "$tag" -R "$OWNER_REPO" -p "$manifest_name" -D "$tmp" --clobber >/dev/null 2>&1; then
      MANIFEST="$tmp/$manifest_name"
      good "manifest $manifest_name downloaded"
    else
      bad "manifest $manifest_name could not be downloaded"
    fi
  else
    bad "release $tag has no $manifest_name asset"
  fi
fi

if [[ -z "$MANIFEST" || ! -f "$MANIFEST" ]]; then
  echo "release verification: $fail failure(s) (no manifest to inspect)"; exit 1
fi

status="$(jq -r '.status // "unknown"' "$MANIFEST")"
failed="$(jq -r '.summary.failed // 0' "$MANIFEST")"
if [[ "$status" == "success" && "$failed" == "0" ]]; then
  good "manifest status=success failed=0"
else
  bad "manifest status=$status failed=$failed"
fi

asset_present() {
  local name="$1"
  if [[ -n "$ASSETS_DIR" ]]; then
    [[ -f "$ASSETS_DIR/$name" ]]
  elif [[ -n "$VERSION" ]]; then
    printf '%s\n' "${published[@]}" | grep -qx "$name"
  else
    return 0
  fi
}

count=0
while IFS=$'\t' read -r name signed sigfile; do
  count=$((count + 1))
  if [[ "$signed" == "true" && -n "$sigfile" && "$sigfile" != "null" ]]; then
    good "$name manifest signed=true signature_file=$sigfile"
  else
    bad "$name manifest signed=$signed signature_file='${sigfile}'"
  fi
  if asset_present "$name.minisig"; then
    good "$name.minisig published"
  else
    bad "$name.minisig is not published"
  fi
  if [[ -n "$ASSETS_DIR" && -f "$ASSETS_DIR/$name" && -f "$ASSETS_DIR/$name.minisig" ]]; then
    if command -v minisign >/dev/null 2>&1; then
      if minisign -Vm "$ASSETS_DIR/$name" -x "$ASSETS_DIR/$name.minisig" -p "$PUBKEY" >/dev/null 2>&1; then
        good "$name signature verifies against release/minisign.pub"
      else
        bad "$name signature does NOT verify against release/minisign.pub"
      fi
    else
      bad "minisign is not installed; cannot verify $name"
    fi
  fi
done < <(jq -r '.artifacts[] | [.name, (.signed|tostring), (.signature_file // "")] | @tsv' "$MANIFEST")
[[ $count -gt 0 ]] || bad "manifest lists no artifacts"

for required in SHA256SUMS SHA256SUMS.minisig; do
  if asset_present "$required"; then
    good "$required published"
  else
    bad "$required is not published"
  fi
done

if [[ $fail -gt 0 ]]; then
  echo "release verification: $fail failure(s) — do not publish/announce; install.sh will reject these artifacts"
  exit 1
fi
echo "release verification: all checks passed ($count artifact(s))"
exit 0
