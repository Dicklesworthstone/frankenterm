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

GENERATOR_VERSION="1.3.0"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="${FT_ATTESTATION_MANIFEST:-$REPO_ROOT/docs/attestations/manifest.json}"
OUT_DIR="${FT_ATTESTATION_OUT_DIR:-$REPO_ROOT/docs/attestations}"
SCHEMA_PATH="${FT_ATTESTATION_SCHEMA:-$REPO_ROOT/docs/attestations/schema.json}"
TAXONOMY_PATH="${FT_PROOF_TAXONOMY:-$REPO_ROOT/docs/proof-taxonomy.json}"
PRIOR_BUNDLE_PATH="${FT_ATTESTATION_PRIOR_BUNDLE:-}"

VERSION=""
CHANNEL="dev"
SIGN_METHOD="unsigned"
ALLOW_PARTIAL=0
STRICT_DEFERRED=0
COSIGN_IDENTITY="${COSIGN_IDENTITY:-}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ED25519_PRIVATE_KEY_PATH="${ED25519_PRIVATE_KEY_PATH:-}"
FT_BEAD_ID="${FT_BEAD_ID:-ft-e87u6.2}"
FT_SCENARIO_ID="${FT_SCENARIO_ID:-attestation_build}"
FT_CORRELATION_ID="${FT_CORRELATION_ID:-}"

usage() {
  cat <<EOF
Usage: $0 --version <semver> [--channel stable|beta|nightly|dev] [--sign cosign|ed25519|unsigned] [--allow-partial] [--strict-deferred]

  --version       Required. Semver, no leading 'v'.
  --channel       Release channel. Default: dev.
  --sign          Signature method. Default: unsigned. cosign requires \$COSIGN_IDENTITY
                  (e.g. the GHA workflow ref); ed25519 requires \$ED25519_PRIVATE_KEY_PATH.
  --allow-partial Permit missing artifact paths in the manifest. Without this the build fails on any null path,
                  which is the desired CI behavior (bridge plan: "CI fails any release that omits a required artifact").
  --strict-deferred
                  Treat any deferred slot as an error. Use for release gates that must not ship a deferred set.
  -h, --help      Show this message.

Environment:
  COSIGN_IDENTITY        Expected SAN/identity for cosign keyless. Required when --sign cosign.
  COSIGN_OIDC_ISSUER     Override OIDC issuer. Default: https://token.actions.githubusercontent.com
  ED25519_PRIVATE_KEY_PATH  PEM-encoded Ed25519 private key for --sign ed25519.
  FT_ATTESTATION_MANIFEST   Override manifest path for tests.
  FT_ATTESTATION_OUT_DIR    Override output directory for tests.
  FT_PROOF_TAXONOMY         Override proof taxonomy registry path.
  FT_ATTESTATION_PRIOR_BUNDLE
                           Optional prior bundle path used to compute taxonomy coverage deltas.
  FT_BEAD_ID / FT_SCENARIO_ID / FT_CORRELATION_ID
                           Structured-log identity fields.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)        VERSION="${2:?--version requires a value}"; shift 2 ;;
    --channel)        CHANNEL="${2:?--channel requires a value}"; shift 2 ;;
    --sign)           SIGN_METHOD="${2:?--sign requires a value}"; shift 2 ;;
    --allow-partial)  ALLOW_PARTIAL=1; shift ;;
    --strict-deferred) STRICT_DEFERRED=1; shift ;;
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
[[ -f "$TAXONOMY_PATH" ]] || { echo "error: proof taxonomy not found: $TAXONOMY_PATH" >&2; exit 1; }

if ! jq -e '.categories | type == "array" and length >= 10' "$TAXONOMY_PATH" >/dev/null; then
  echo "error: proof taxonomy must define at least the 10 core categories: $TAXONOMY_PATH" >&2
  exit 1
fi

if [[ "$TAXONOMY_PATH" == "$REPO_ROOT/"* ]]; then
  TAXONOMY_REL_PATH="${TAXONOMY_PATH#"$REPO_ROOT"/}"
else
  TAXONOMY_REL_PATH="$TAXONOMY_PATH"
fi

GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"
GIT_TREE="$(git -C "$REPO_ROOT" rev-parse "HEAD^{tree}")"
GIT_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
GENERATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
TAG="v${VERSION}"
if [[ "$FT_SCENARIO_ID" == "attestation_build" ]]; then
  FT_SCENARIO_ID="attestation_build_${VERSION}"
fi
if [[ -z "$FT_CORRELATION_ID" ]]; then
  FT_CORRELATION_ID="${FT_BEAD_ID}-${FT_SCENARIO_ID}-${GENERATED_AT}"
fi

build_log() {
  echo "[build] $*" >&2
}

emit_build_json() {
  local step="$1"
  local outcome="$2"
  local reason_code="$3"
  local error_code="$4"
  local slot_category="${5:-}"
  local deferred_to_bead="${6:-}"
  local message="${7:-}"
  local event
  event="$(jq -cn \
    --arg timestamp "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg bead_id "$FT_BEAD_ID" \
    --arg scenario_id "$FT_SCENARIO_ID" \
    --arg surface "scripts/attestation-build.sh" \
    --arg step "$step" \
    --arg outcome "$outcome" \
    --arg reason_code "$reason_code" \
    --arg error_code "$error_code" \
    --arg slot_category "$slot_category" \
    --arg deferred_to_bead "$deferred_to_bead" \
    --arg correlation_id "$FT_CORRELATION_ID" \
    --arg message "$message" \
    '{
      timestamp: $timestamp,
      bead_id: $bead_id,
      scenario_id: $scenario_id,
      surface: $surface,
      step: $step,
      outcome: $outcome,
      reason_code: $reason_code,
      error_code: $error_code,
      slot_category: (if $slot_category == "" then null else $slot_category end),
      deferred_to_bead: (if $deferred_to_bead == "" then null else $deferred_to_bead end),
      correlation_id: $correlation_id,
      message: $message
    }')"
  echo "[build:json] ${event}" >&2
}

CANONICAL_REQUIRED_JSON="$(jq -c '.required_categories' "$MANIFEST")"
SLOTS_JSON="$(jq -c '.slots' "$MANIFEST")"

unknown_proof_categories="$(jq -rn \
  --slurpfile taxonomy "$TAXONOMY_PATH" \
  --argjson slots "$SLOTS_JSON" \
  '($taxonomy[0].categories // [] | map(.id)) as $ids
   | [($slots[]?.proof_categories // [])[] | select(($ids | index(.)) | not)] | unique | map(tostring) | join(", ")')"
if [[ -n "$unknown_proof_categories" ]]; then
  echo "error: manifest references unknown proof taxonomy id(s): $unknown_proof_categories" >&2
  exit 1
fi

# Enumerate slots, hash each non-null artifact path. Track unfilled categories.
declare -a artifact_objs=()
declare -a deferred_objs=()
declare -a deferred_categories=()
unfilled=()
slot_count="$(jq 'length' <<<"$SLOTS_JSON")"

for ((i=0; i<slot_count; i++)); do
  slot="$(jq -c ".[$i]" <<<"$SLOTS_JSON")"
  category="$(jq -r '.category' <<<"$slot")"
  path="$(jq -r '.path // ""' <<<"$slot")"
  media_type="$(jq -r '.media_type' <<<"$slot")"
  produced_by_bead="$(jq -r '.produced_by_bead // ""' <<<"$slot")"
  deferred_to_bead="$(jq -r '.deferred_to_bead // ""' <<<"$slot")"
  deferred_reason="$(jq -r '.deferred_reason // ""' <<<"$slot")"
  description="$(jq -r '.description // ""' <<<"$slot")"
  proof_categories_json="$(jq -c '.proof_categories // []' <<<"$slot")"

  if [[ -z "$path" ]]; then
    if [[ -n "$deferred_to_bead" ]]; then
      deferred_obj="$(jq -n \
        --arg category "$category" \
        --arg media_type "$media_type" \
        --arg produced_by_bead "$produced_by_bead" \
        --arg deferred_to_bead "$deferred_to_bead" \
        --arg deferred_reason "$deferred_reason" \
        --arg description "$description" \
        --argjson proof_categories "$proof_categories_json" \
        '{category:$category, media_type:$media_type, produced_by_bead:$produced_by_bead,
          deferred_to_bead:$deferred_to_bead, deferred_reason:$deferred_reason,
          proof_categories:$proof_categories}
         + (if $description != "" then {description:$description} else {} end)')"
      deferred_objs+=("$deferred_obj")
      deferred_categories+=("$category")
      emit_build_json "warn.deferred" "warned" "slot_deferred" "none" "$category" "$deferred_to_bead" "$deferred_reason"
      continue
    fi
    unfilled+=("$category (produced by ${produced_by_bead:-?})")
    emit_build_json "error.unfilled" "failed" "slot_unfilled" "ATTESTATION-SLOT-UNFILLED" "$category" "" "slot has path=null with no deferred_to_bead"
    continue
  fi

  abs_path="$REPO_ROOT/$path"
  if [[ ! -f "$abs_path" ]]; then
    unfilled+=("$category (missing path $path, produced by ${produced_by_bead:-?})")
    emit_build_json "error.missing_artifact" "failed" "artifact_missing" "ATTESTATION-ARTIFACT-MISSING" "$category" "" "$path"
    if [[ $ALLOW_PARTIAL -eq 0 ]]; then
      build_log "error: artifact missing on disk: $path (declared for category $category, bead $produced_by_bead)"
      exit 1
    fi
    continue
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
    --argjson proof_categories "$proof_categories_json" \
    '{category:$category, path:$path, media_type:$media_type, sha256:$sha256, size_bytes:$size_bytes}
     + (if $produced_by_bead != "" then {produced_by_bead:$produced_by_bead} else {} end)
     + {proof_categories:$proof_categories}
     + (if $description       != "" then {description:$description}             else {} end)')"
  artifact_objs+=("$obj")
  emit_build_json "validate.${category}" "passed" "artifact_hashed" "none" "$category" "" "$path"
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
    deferred=0
    for deferred_cat in "${deferred_categories[@]}"; do
      if [[ "$deferred_cat" == "$cat" ]]; then deferred=1; break; fi
    done
    if [[ $deferred -eq 1 ]]; then
      continue
    fi
    missing_required+=("$cat")
  fi
done

if [[ ${#deferred_categories[@]} -gt 0 ]]; then
  build_log "warning: deferred attestation slots present:"
  for obj in "${deferred_objs[@]}"; do
    category="$(jq -r '.category' <<<"$obj")"
    bead="$(jq -r '.deferred_to_bead' <<<"$obj")"
    reason="$(jq -r '.deferred_reason' <<<"$obj")"
    build_log "  - $category deferred to $bead: $reason"
  done
  if [[ $STRICT_DEFERRED -eq 1 ]]; then
    emit_build_json "summary" "failed" "strict_deferred" "ATTESTATION-DEFERRED-SLOT" "" "" "strict deferred mode rejected ${#deferred_categories[@]} deferred slot(s)"
    build_log "error: --strict-deferred rejects ${#deferred_categories[@]} deferred slot(s)"
    exit 1
  fi
fi

if [[ ${#missing_required[@]} -gt 0 ]]; then
  build_log "error: canonical required categories missing from bundle:"
  for c in "${missing_required[@]}"; do build_log "  - $c"; done
  if [[ $ALLOW_PARTIAL -eq 0 ]]; then
    build_log "  hint: producing beads have not yet shipped — see docs/reality-check-bridge-plan.md"
    build_log "  to build a partial bundle anyway (dev channel only) pass --allow-partial"
    emit_build_json "summary" "failed" "missing_required" "ATTESTATION-REQUIRED-MISSING" "" "" "${#missing_required[@]} required category/categories missing"
    exit 1
  else
    emit_build_json "summary" "warned" "allow_partial_missing_required" "none" "" "" "${#missing_required[@]} required category/categories omitted by --allow-partial"
  fi
  if [[ "$CHANNEL" != "dev" ]]; then
    build_log "error: --allow-partial only permitted on --channel dev (got: $CHANNEL)"
    exit 1
  fi
  build_log "warning: building partial bundle on channel=dev. Bundle's required_categories will list only delivered categories."
fi

# The bundle commits to delivering only what it actually includes. verify --strict-required
# compares this list against manifest.json's canonical list to flag production-release gaps.
if [[ ${#delivered_categories[@]} -eq 0 ]]; then
  REQUIRED_CATEGORIES_JSON="[]"
else
  REQUIRED_CATEGORIES_JSON="$(printf '%s\n' "${delivered_categories[@]}" | jq -R -c -s 'split("\n") | map(select(. != ""))')"
fi

# Assemble bundle (without signature, then add signature placeholder).
if [[ ${#artifact_objs[@]} -eq 0 ]]; then
  artifacts_json="[]"
else
  artifacts_json="$(printf '%s\n' "${artifact_objs[@]}" | jq -c -s '.')"
fi
if [[ ${#deferred_objs[@]} -eq 0 ]]; then
  deferred_slots_json="[]"
else
  deferred_slots_json="$(printf '%s\n' "${deferred_objs[@]}" | jq -c -s '.')"
fi

if [[ -n "$PRIOR_BUNDLE_PATH" && -f "$PRIOR_BUNDLE_PATH" ]]; then
  PRIOR_COVERAGE_JSON="$(jq -c '.taxonomy_coverage // null' "$PRIOR_BUNDLE_PATH")"
  PRIOR_COVERAGE_STATUS="computed"
elif [[ -n "$PRIOR_BUNDLE_PATH" ]]; then
  PRIOR_COVERAGE_JSON="null"
  PRIOR_COVERAGE_STATUS="prior_bundle_missing"
else
  PRIOR_COVERAGE_JSON="null"
  PRIOR_COVERAGE_STATUS="no_prior_bundle"
fi

taxonomy_coverage_json="$(jq -n \
  --slurpfile taxonomy "$TAXONOMY_PATH" \
  --arg taxonomy_path "$TAXONOMY_REL_PATH" \
  --arg prior_status "$PRIOR_COVERAGE_STATUS" \
  --argjson prior_coverage "$PRIOR_COVERAGE_JSON" \
  --argjson artifacts "$artifacts_json" \
  --argjson deferred_slots "$deferred_slots_json" \
  '($taxonomy[0].categories // []) as $categories
   | def coverage_count($items; $id):
       [$items[]? | select((.proof_categories // []) | index($id))] | length;
     def prior_count($id; $field):
       if $prior_coverage == null then 0
       else (($prior_coverage.category_counts // [])
         | map(select(.id == $id)) | .[0][$field] // 0)
       end;
     ($categories
       | map(. as $category
         | (coverage_count($artifacts; $category.id)) as $artifact_count
         | (coverage_count($deferred_slots; $category.id)) as $deferred_count
         | {
             id: $category.id,
             slug: $category.slug,
             name: $category.name,
             bridge_plan_core: ($category.bridge_plan_core // false),
             artifact_count: $artifact_count,
             deferred_slot_count: $deferred_count,
             below_threshold: (($artifact_count + $deferred_count) < 1)
           })) as $category_counts
   | {
       schema_version: "1.0.0",
       taxonomy_path: $taxonomy_path,
       category_counts: $category_counts,
       below_threshold_count: ($category_counts | map(select(.below_threshold)) | length),
       uncategorized_artifact_count: ([$artifacts[]? | select((.proof_categories // []) | length == 0)] | length),
       delta_from_prior_release: {
         status: $prior_status,
         category_deltas: ($category_counts
           | map({
               id,
               slug,
               artifact_delta: (.artifact_count - prior_count(.id; "artifact_count")),
               deferred_slot_delta: (.deferred_slot_count - prior_count(.id; "deferred_slot_count"))
             }))
       }
     }')"

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
  --argjson deferred_slots "$deferred_slots_json" \
  --argjson taxonomy_coverage "$taxonomy_coverage_json" \
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
    required_categories: $required_categories,
    deferred_slots: $deferred_slots,
    taxonomy_coverage: $taxonomy_coverage
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
    sigstore_hash="$(sha256_file "$sig_path")"
    sigstore_size="$(wc -c < "$sig_path" | tr -d ' ')"

    sig_obj="$(jq -n \
      --arg method "sigstore-cosign-keyless" \
      --arg canonical_sha256 "$canonical_sha" \
      --arg sigstore_path "docs/attestations/${VERSION}.sigstore" \
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
      echo "error: --sign unsigned only permitted on --channel dev (got: $CHANNEL). Production releases must use --sign cosign or --sign ed25519." >&2
      exit 1
    fi
    reason_text="dev channel bundle — production releases must use --sign cosign or --sign ed25519"
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
echo "  taxonomy       : $(jq -r '.below_threshold_count' <<<"$taxonomy_coverage_json") below threshold"
if [[ ${#deferred_objs[@]} -gt 0 ]]; then
  echo "  deferred slots : ${#deferred_objs[@]}"
fi
if [[ ${#unfilled[@]} -gt 0 ]]; then
  echo "  unfilled slots : ${#unfilled[@]}"
  for u in "${unfilled[@]}"; do echo "    - $u"; done
fi
echo "  signature      : $SIGN_METHOD"
emit_build_json "summary" "passed" "bundle_written" "none" "" "" "$out_path"
echo "  canonical_sha  : $canonical_sha"
