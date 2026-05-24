#!/usr/bin/env bash
# scripts/retract-bundle-slot.sh — create a signed attestation slot retraction.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RETRACTIONS_ROOT="${FT_ATTESTATION_RETRACTIONS_ROOT:-$REPO_ROOT/docs/attestations/retractions}"
BUNDLE=""
SLOT=""
RATIONALE_FILE=""
CORRECTED_CLAIM_FILE=""
RETRACTED_BY_RELEASE=""
SIGN_METHOD="ed25519"
COSIGN_IDENTITY="${COSIGN_IDENTITY:-}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ED25519_PRIVATE_KEY_PATH="${ED25519_PRIVATE_KEY_PATH:-}"

usage() {
  cat <<EOF
Usage: $0 --bundle <bundle.json> --slot <category> --rationale-file <path> --retracted-by-release <version> [--corrected-claim-file <path>] [--sign ed25519|cosign]

Creates:
  <retractions-root>/<bundle-sha256>/<slot-name>.json

Environment:
  FT_ATTESTATION_RETRACTIONS_ROOT  Override output root. Defaults to docs/attestations/retractions.
  ED25519_PRIVATE_KEY_PATH         PEM Ed25519 private key for --sign ed25519.
  COSIGN_IDENTITY                  Expected certificate identity for --sign cosign.
  COSIGN_OIDC_ISSUER               OIDC issuer for --sign cosign. Defaults to GitHub Actions.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle)               BUNDLE="${2:?--bundle requires a value}"; shift 2 ;;
    --slot)                 SLOT="${2:?--slot requires a value}"; shift 2 ;;
    --rationale-file)       RATIONALE_FILE="${2:?--rationale-file requires a value}"; shift 2 ;;
    --corrected-claim-file) CORRECTED_CLAIM_FILE="${2:?--corrected-claim-file requires a value}"; shift 2 ;;
    --retracted-by-release) RETRACTED_BY_RELEASE="${2:?--retracted-by-release requires a value}"; shift 2 ;;
    --sign)                 SIGN_METHOD="${2:?--sign requires a value}"; shift 2 ;;
    -h|--help)              usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$BUNDLE" ]] || { echo "error: --bundle is required" >&2; exit 2; }
[[ -n "$SLOT" ]] || { echo "error: --slot is required" >&2; exit 2; }
[[ -n "$RATIONALE_FILE" ]] || { echo "error: --rationale-file is required" >&2; exit 2; }
[[ -n "$RETRACTED_BY_RELEASE" ]] || { echo "error: --retracted-by-release is required" >&2; exit 2; }

if [[ ! -f "$BUNDLE" ]]; then
  if [[ -f "$REPO_ROOT/$BUNDLE" ]]; then
    BUNDLE="$REPO_ROOT/$BUNDLE"
  else
    echo "error: bundle not found: $BUNDLE" >&2
    exit 2
  fi
fi
[[ -f "$RATIONALE_FILE" ]] || { echo "error: rationale file not found: $RATIONALE_FILE" >&2; exit 2; }
if [[ -n "$CORRECTED_CLAIM_FILE" && ! -f "$CORRECTED_CLAIM_FILE" ]]; then
  echo "error: corrected claim file not found: $CORRECTED_CLAIM_FILE" >&2
  exit 2
fi

case "$SIGN_METHOD" in
  ed25519|cosign) ;;
  *) echo "error: --sign must be one of ed25519|cosign (got: $SIGN_METHOD)" >&2; exit 2 ;;
esac

command -v jq >/dev/null 2>&1 || { echo "error: jq is required" >&2; exit 1; }

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
repo_relative_path() {
  local path="$1"
  if [[ "$path" == "$REPO_ROOT/"* ]]; then
    printf '%s\n' "${path#"$REPO_ROOT"/}"
  elif [[ "$path" != /* ]]; then
    printf '%s\n' "$path"
  else
    echo "error: signed retraction output must be inside the repository so the verifier can resolve it: $path" >&2
    exit 1
  fi
}

bundle_json="$(cat "$BUNDLE")"
bundle_sha="$(sha256_file "$BUNDLE")"
original_claim_value="$(jq -c --arg slot "$SLOT" '
  ([.artifacts[]? | select(.category == $slot)] + [.deferred_slots[]? | select(.category == $slot)])[0] // empty
' <<<"$bundle_json")"
if [[ -z "$original_claim_value" ]]; then
  echo "error: slot not found in bundle artifacts/deferred_slots: $SLOT" >&2
  exit 1
fi

rationale_json="$(jq -Rs . < "$RATIONALE_FILE")"
corrected_claim_json="null"
if [[ -n "$CORRECTED_CLAIM_FILE" ]]; then
  corrected_claim_json="$(jq -c '.' "$CORRECTED_CLAIM_FILE")"
fi

safe_slot="${SLOT//\//__}"
safe_slot="${safe_slot//:/__}"
out_dir="$RETRACTIONS_ROOT/$bundle_sha"
out_path="$out_dir/${safe_slot}.json"
mkdir -p "$out_dir"

retraction_no_sig="$(jq -n \
  --arg schema_version "1.0.0" \
  --arg retracted_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg retracted_by_release "$RETRACTED_BY_RELEASE" \
  --arg affected_slot "$SLOT" \
  --arg original_bundle_sha256 "$bundle_sha" \
  --argjson retraction_rationale "$rationale_json" \
  --argjson original_claim_value "$original_claim_value" \
  --argjson corrected_claim_value "$corrected_claim_json" \
  '{
    schema_version: $schema_version,
    retracted_at: $retracted_at,
    retracted_by_release: $retracted_by_release,
    retraction_rationale: $retraction_rationale,
    affected_slot: $affected_slot,
    original_bundle_sha256: $original_bundle_sha256,
    original_claim_value: $original_claim_value,
    corrected_claim_value: $corrected_claim_value
  }')"
canonical_payload="$(jq -S -c '.' <<<"$retraction_no_sig")"
canonical_sha="$(printf '%s' "$canonical_payload" | sha256_stdin)"

case "$SIGN_METHOD" in
  ed25519)
    command -v openssl >/dev/null 2>&1 || { echo "error: openssl is required for --sign ed25519" >&2; exit 1; }
    command -v xxd >/dev/null 2>&1 || { echo "error: xxd is required for --sign ed25519" >&2; exit 1; }
    [[ -n "$ED25519_PRIVATE_KEY_PATH" ]] || { echo "error: ED25519_PRIVATE_KEY_PATH is required for --sign ed25519" >&2; exit 1; }
    [[ -f "$ED25519_PRIVATE_KEY_PATH" ]] || { echo "error: ED25519_PRIVATE_KEY_PATH not found: $ED25519_PRIVATE_KEY_PATH" >&2; exit 1; }
    public_key="$(openssl pkey -in "$ED25519_PRIVATE_KEY_PATH" -pubout -outform DER | tail -c 32 | xxd -p -c 256)"
    if [[ ${#public_key} -ne 64 || "$public_key" =~ [^0-9A-Fa-f] ]]; then
      echo "error: failed to derive a 32-byte Ed25519 public key from $ED25519_PRIVATE_KEY_PATH" >&2
      exit 1
    fi
    canonical_path="$out_dir/${safe_slot}.canonical.payload"
    sig_path="$out_dir/${safe_slot}.ed25519.sig.hex"
    printf '%s' "$canonical_payload" > "$canonical_path"
    openssl pkeyutl -sign -rawin -inkey "$ED25519_PRIVATE_KEY_PATH" -in "$canonical_path" \
      | xxd -p -c 256 > "$sig_path"
    sig_rel_path="$(repo_relative_path "$sig_path")"
    sig_obj="$(jq -n \
      --arg method "ed25519" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg signature_path "$sig_rel_path" \
      --arg public_key "$public_key" \
      '{method:$method, canonical_sha256:$canonical_sha256, signature_path:$signature_path, public_key:$public_key}')"
    ;;
  cosign)
    command -v cosign >/dev/null 2>&1 || { echo "error: cosign is required for --sign cosign" >&2; exit 1; }
    [[ -n "$COSIGN_IDENTITY" ]] || { echo "error: COSIGN_IDENTITY is required for --sign cosign" >&2; exit 1; }
    canonical_path="$out_dir/${safe_slot}.canonical.payload"
    sigstore_path="$out_dir/${safe_slot}.sigstore"
    printf '%s' "$canonical_payload" > "$canonical_path"
    cosign sign-blob --yes --bundle "$sigstore_path" "$canonical_path"
    sigstore_hash="$(sha256_file "$sigstore_path")"
    sigstore_size="$(wc -c < "$sigstore_path" | tr -d ' ')"
    sigstore_rel_path="$(repo_relative_path "$sigstore_path")"
    sig_obj="$(jq -n \
      --arg method "sigstore-cosign-keyless" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg sigstore_path "$sigstore_rel_path" \
      --arg sigstore_sha256 "$sigstore_hash" \
      --argjson sigstore_size_bytes "$sigstore_size" \
      --arg certificate_identity "$COSIGN_IDENTITY" \
      --arg certificate_oidc_issuer "$COSIGN_OIDC_ISSUER" \
      '{
        method: $method,
        canonical_sha256: $canonical_sha256,
        sigstore_bundle: {
          path: $sigstore_path,
          sha256: $sigstore_sha256,
          size_bytes: $sigstore_size_bytes
        },
        certificate_identity: $certificate_identity,
        certificate_oidc_issuer: $certificate_oidc_issuer
      }')"
    ;;
esac

final_retraction="$(jq --argjson sig "$sig_obj" '. + {retraction_signature: $sig}' <<<"$retraction_no_sig")"
printf '%s\n' "$final_retraction" | jq -S '.' > "$out_path"

echo "wrote $out_path"
echo "  original_bundle_sha256 : $bundle_sha"
echo "  affected_slot          : $SLOT"
echo "  signature              : $SIGN_METHOD"
echo "  canonical_sha          : $canonical_sha"
echo "  next                   : rebuild the next bundle so its retractions field lists this record"
