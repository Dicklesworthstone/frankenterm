#!/usr/bin/env bash
# scripts/attestation-build.sh — build a release attestation bundle.
#
# Reads docs/attestations/manifest.json, hashes every declared artifact, and
# emits docs/attestations/<version>.json conforming to docs/attestations/schema.json.
#
# Defined by ft-syqcz.1 (BR-RC-FOUNDATION.G3.1). See docs/reality-check-bridge-plan.md.
#
# Usage:
#   scripts/attestation-build.sh --version 0.2.0
#   scripts/attestation-build.sh --version 0.2.0 --channel stable --sign cosign
#   ED25519_PRIVATE_KEY_PATH=release-ed25519.pem scripts/attestation-build.sh --version 0.2.0 --sign ed25519
#   scripts/attestation-build.sh --version 0.2.0 --channel dev   --sign unsigned --allow-partial
#
# Required tools: bash, jq, git, sha256sum (or shasum -a 256). cosign required for --sign cosign.
# openssl + xxd required for --sign ed25519.

set -euo pipefail

GENERATOR_VERSION="1.0.0"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$REPO_ROOT/docs/attestations/manifest.json"
OUT_DIR="$REPO_ROOT/docs/attestations"
SCHEMA_PATH="$REPO_ROOT/docs/attestations/schema.json"

VERSION=""
CHANNEL="dev"
SIGN_METHOD="unsigned"
ALLOW_PARTIAL=0
COSIGN_IDENTITY="${COSIGN_IDENTITY:-}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ED25519_PRIVATE_KEY_PATH="${ED25519_PRIVATE_KEY_PATH:-}"

usage() {
  cat <<EOF
Usage: $0 --version <semver> [--channel stable|beta|nightly|dev] [--sign cosign|ed25519|unsigned] [--allow-partial]

  --version       Required. Semver, no leading 'v'.
  --channel       Release channel. Default: dev.
  --sign          Signature method. Default: unsigned. cosign requires \$COSIGN_IDENTITY
                  (e.g. the GHA workflow ref); ed25519 requires \$ED25519_PRIVATE_KEY_PATH.
  --allow-partial Permit missing artifact paths in the manifest. Without this the build fails on any null path,
                  which is the desired CI behavior (bridge plan: "CI fails any release that omits a required artifact").
  -h, --help      Show this message.

Environment:
  COSIGN_IDENTITY        Expected SAN/identity for cosign keyless. Required when --sign cosign.
  COSIGN_OIDC_ISSUER     Override OIDC issuer. Default: https://token.actions.githubusercontent.com
  ED25519_PRIVATE_KEY_PATH  PEM-encoded Ed25519 private key for --sign ed25519.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)        VERSION="${2:?--version requires a value}"; shift 2 ;;
    --channel)        CHANNEL="${2:?--channel requires a value}"; shift 2 ;;
    --sign)           SIGN_METHOD="${2:?--sign requires a value}"; shift 2 ;;
    --allow-partial)  ALLOW_PARTIAL=1; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$VERSION" ]] || { echo "error: --version is required" >&2; exit 2; }

case "$CHANNEL" in
  stable|beta|nightly|dev) ;;
  *) echo "error: --channel must be one of stable|beta|nightly|dev (got: $CHANNEL)" >&2; exit 2 ;;
esac

case "$SIGN_METHOD" in
  cosign|ed25519|unsigned) ;;
  *) echo "error: --sign must be one of cosign|ed25519|unsigned (got: $SIGN_METHOD)" >&2; exit 2 ;;
esac

command -v jq  >/dev/null 2>&1 || { echo "error: jq is required" >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo "error: git is required" >&2; exit 1; }

# Portable sha256: GNU sha256sum on Linux, shasum -a 256 on macOS/BSD.
sha256_file() {
  local f="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" | awk '{print $1}'
  else
    shasum -a 256 "$f" | awk '{print $1}'
  fi
}
sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

[[ -f "$MANIFEST" ]] || { echo "error: manifest not found: $MANIFEST" >&2; exit 1; }
[[ -f "$SCHEMA_PATH" ]] || { echo "error: schema not found: $SCHEMA_PATH" >&2; exit 1; }

GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"
GIT_TREE="$(git -C "$REPO_ROOT" rev-parse "HEAD^{tree}")"
GIT_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
GENERATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
TAG="v${VERSION}"

CANONICAL_REQUIRED_JSON="$(jq -c '.required_categories' "$MANIFEST")"
SLOTS_JSON="$(jq -c '.slots' "$MANIFEST")"

# Enumerate slots, hash each non-null artifact path. Track unfilled categories.
declare -a artifact_objs=()
unfilled=()
slot_count="$(jq 'length' <<<"$SLOTS_JSON")"

for ((i=0; i<slot_count; i++)); do
  slot="$(jq -c ".[$i]" <<<"$SLOTS_JSON")"
  category="$(jq -r '.category' <<<"$slot")"
  path="$(jq -r '.path // ""' <<<"$slot")"
  media_type="$(jq -r '.media_type' <<<"$slot")"
  produced_by_bead="$(jq -r '.produced_by_bead // ""' <<<"$slot")"
  description="$(jq -r '.description // ""' <<<"$slot")"

  if [[ -z "$path" ]]; then
    unfilled+=("$category (produced by ${produced_by_bead:-?})")
    continue
  fi

  abs_path="$REPO_ROOT/$path"
  if [[ ! -f "$abs_path" ]]; then
    echo "error: artifact missing on disk: $path (declared for category $category, bead $produced_by_bead)" >&2
    exit 1
  fi

  hash="$(sha256_file "$abs_path")"
  size="$(wc -c < "$abs_path" | tr -d ' ')"

  obj="$(jq -n \
    --arg category "$category" \
    --arg path "$path" \
    --arg media_type "$media_type" \
    --arg sha256 "$hash" \
    --argjson size_bytes "$size" \
    --arg produced_by_bead "$produced_by_bead" \
    --arg description "$description" \
    '{category:$category, path:$path, media_type:$media_type, sha256:$sha256, size_bytes:$size_bytes}
     + (if $produced_by_bead != "" then {produced_by_bead:$produced_by_bead} else {} end)
     + (if $description       != "" then {description:$description}             else {} end)')"
  artifact_objs+=("$obj")
done

# Required-category gate. Production releases must satisfy every canonical category.
canonical_count="$(jq 'length' <<<"$CANONICAL_REQUIRED_JSON")"
declare -a missing_required=()
declare -a delivered_categories=()
for ((i=0; i<canonical_count; i++)); do
  cat="$(jq -r ".[$i]" <<<"$CANONICAL_REQUIRED_JSON")"
  found=0
  for obj in "${artifact_objs[@]}"; do
    obj_cat="$(jq -r '.category' <<<"$obj")"
    if [[ "$obj_cat" == "$cat" ]]; then found=1; break; fi
  done
  if [[ $found -eq 1 ]]; then
    delivered_categories+=("$cat")
  else
    missing_required+=("$cat")
  fi
done

if [[ ${#missing_required[@]} -gt 0 ]]; then
  echo "error: canonical required categories missing from bundle:" >&2
  for c in "${missing_required[@]}"; do echo "  - $c" >&2; done
  if [[ $ALLOW_PARTIAL -eq 0 ]]; then
    echo "  hint: producing beads have not yet shipped — see docs/reality-check-bridge-plan.md" >&2
    echo "  to build a partial bundle anyway (dev channel only) pass --allow-partial" >&2
    exit 1
  fi
  if [[ "$CHANNEL" != "dev" ]]; then
    echo "error: --allow-partial only permitted on --channel dev (got: $CHANNEL)" >&2
    exit 1
  fi
  echo "warning: building partial bundle on channel=dev. Bundle's required_categories will list only delivered categories." >&2
fi

# The bundle commits to delivering only what it actually includes. verify --strict-required
# compares this list against manifest.json's canonical list to flag production-release gaps.
if [[ ${#delivered_categories[@]} -eq 0 ]]; then
  REQUIRED_CATEGORIES_JSON="[]"
else
  REQUIRED_CATEGORIES_JSON="$(printf '%s\n' "${delivered_categories[@]}" | jq -R -c -s 'split("\n") | map(select(. != ""))')"
fi

# Assemble bundle (without signature, then add signature placeholder).
artifacts_json="$(printf '%s\n' "${artifact_objs[@]}" | jq -c -s '.')"

bundle_no_sig="$(jq -n \
  --arg schema_version "1.0.0" \
  --arg version "$VERSION" \
  --arg tag "$TAG" \
  --arg channel "$CHANNEL" \
  --arg generated_at "$GENERATED_AT" \
  --arg generator_version "$GENERATOR_VERSION" \
  --arg commit "$GIT_COMMIT" \
  --arg tree "$GIT_TREE" \
  --arg branch "$GIT_BRANCH" \
  --argjson artifacts "$artifacts_json" \
  --argjson required_categories "$REQUIRED_CATEGORIES_JSON" \
  '{
    schema_version: $schema_version,
    release: {
      version: $version,
      tag: $tag,
      channel: $channel
    },
    generated_at: $generated_at,
    generator: {
      name: "scripts/attestation-build.sh",
      version: $generator_version
    },
    git: ({commit: $commit, tree: $tree} + (if $branch != "" then {branch: $branch} else {} end)),
    artifacts: $artifacts,
    required_categories: $required_categories
  }')"

# Canonical signing payload: bundle (without .signature) sorted-keys + compact.
canonical_payload="$(jq -S -c '.' <<<"$bundle_no_sig")"
canonical_sha="$(printf '%s' "$canonical_payload" | sha256_stdin)"

mkdir -p "$OUT_DIR"

case "$SIGN_METHOD" in
  cosign)
    command -v cosign >/dev/null 2>&1 || { echo "error: cosign not installed; pass --sign unsigned or install sigstore/cosign" >&2; exit 1; }
    [[ -n "$COSIGN_IDENTITY" ]] || { echo "error: COSIGN_IDENTITY env var required for --sign cosign (e.g. the GHA workflow ref)" >&2; exit 1; }

    canon_path="$OUT_DIR/${VERSION}.canonical.json"
    sig_path="$OUT_DIR/${VERSION}.sigstore"
    printf '%s' "$canonical_payload" > "$canon_path"
    # Cosign keyless requires GHA OIDC. Outputs a sigstore bundle.
    cosign sign-blob \
      --yes \
      --bundle "$sig_path" \
      "$canon_path"

    sig_obj="$(jq -n \
      --arg method "sigstore-cosign-keyless" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg bundle_path "docs/attestations/${VERSION}.sigstore" \
      --arg certificate_identity "$COSIGN_IDENTITY" \
      --arg certificate_oidc_issuer "$COSIGN_OIDC_ISSUER" \
      '{method:$method, canonical_sha256:$canonical_sha256, bundle_path:$bundle_path, certificate_identity:$certificate_identity, certificate_oidc_issuer:$certificate_oidc_issuer}')"
    ;;
  ed25519)
    command -v openssl >/dev/null 2>&1 || { echo "error: openssl not installed; required for --sign ed25519" >&2; exit 1; }
    command -v xxd >/dev/null 2>&1 || { echo "error: xxd not installed; required for --sign ed25519" >&2; exit 1; }
    [[ -n "$ED25519_PRIVATE_KEY_PATH" ]] || { echo "error: ED25519_PRIVATE_KEY_PATH env var required for --sign ed25519" >&2; exit 1; }
    [[ -f "$ED25519_PRIVATE_KEY_PATH" ]] || { echo "error: ED25519_PRIVATE_KEY_PATH file not found: $ED25519_PRIVATE_KEY_PATH" >&2; exit 1; }
    if ! openssl pkey -in "$ED25519_PRIVATE_KEY_PATH" -text -noout 2>/dev/null | head -n 1 | grep -q '^ED25519 Private-Key:'; then
      echo "error: ED25519_PRIVATE_KEY_PATH must point to a PEM-encoded Ed25519 private key" >&2
      exit 1
    fi

    canon_path="$OUT_DIR/${VERSION}.canonical.json"
    sig_path="$OUT_DIR/${VERSION}.ed25519.sig.hex"
    pubkey_der_tmp="$(mktemp)"
    sig_bin_tmp="$(mktemp)"
    printf '%s' "$canonical_payload" > "$canon_path"
    openssl pkey -in "$ED25519_PRIVATE_KEY_PATH" -pubout -outform DER > "$pubkey_der_tmp"
    public_key="$(tail -c 32 "$pubkey_der_tmp" | xxd -p -c 256)"
    if [[ ${#public_key} -ne 64 || "$public_key" =~ [^0-9A-Fa-f] ]]; then
      echo "error: failed to derive a 32-byte Ed25519 public key from $ED25519_PRIVATE_KEY_PATH" >&2
      rm -f "$pubkey_der_tmp" "$sig_bin_tmp"
      exit 1
    fi
    openssl pkeyutl -sign -rawin -inkey "$ED25519_PRIVATE_KEY_PATH" -in "$canon_path" -out "$sig_bin_tmp"
    xxd -p -c 256 "$sig_bin_tmp" > "$sig_path"
    rm -f "$pubkey_der_tmp" "$sig_bin_tmp"

    sig_obj="$(jq -n \
      --arg method "ed25519" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg signature_path "docs/attestations/${VERSION}.ed25519.sig.hex" \
      --arg public_key "$public_key" \
      '{method:$method, canonical_sha256:$canonical_sha256, signature_path:$signature_path, public_key:$public_key}')"
    ;;
  unsigned)
    if [[ "$CHANNEL" != "dev" ]]; then
      echo "error: --sign unsigned only permitted on --channel dev (got: $CHANNEL). Production releases must use --sign cosign." >&2
      exit 1
    fi
    reason_text="dev channel bundle — sigstore signing wired in CI under ft-syqcz.1; production releases must use --sign cosign"
    sig_obj="$(jq -n \
      --arg method "unsigned" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg reason "$reason_text" \
      '{method:$method, canonical_sha256:$canonical_sha256, reason:$reason}')"
    ;;
esac

final_bundle="$(jq --argjson sig "$sig_obj" '. + {signature: $sig}' <<<"$bundle_no_sig")"
out_path="$OUT_DIR/${VERSION}.json"
printf '%s\n' "$final_bundle" | jq -S '.' > "$out_path"

echo "wrote $out_path"
echo "  schema_version : 1.0.0"
echo "  release        : v${VERSION} (${CHANNEL})"
echo "  git            : ${GIT_COMMIT}"
echo "  artifacts      : ${#artifact_objs[@]}"
if [[ ${#unfilled[@]} -gt 0 ]]; then
  echo "  unfilled slots : ${#unfilled[@]}"
  for u in "${unfilled[@]}"; do echo "    - $u"; done
fi
echo "  signature      : $SIGN_METHOD"
echo "  canonical_sha  : $canonical_sha"
