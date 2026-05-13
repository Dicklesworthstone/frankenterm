#!/usr/bin/env bash
# scripts/attestation-verify.sh — verify a release attestation bundle.
#
# Re-derives every artifact's SHA-256 from disk, recomputes the canonical
# signing payload, and (when the bundle is signed) verifies the signature.
#
# Defined by ft-syqcz.1 (BR-RC-FOUNDATION.G3.1).
#
# Usage:
#   scripts/attestation-verify.sh docs/attestations/0.2.0.json
#   scripts/attestation-verify.sh docs/attestations/0.2.0.json --json
#
# Exit code: 0 on full pass, 1 on any failure, 2 on usage error, 3 when the
# bundle is validly retracted.
# JSON output (with --json) is machine-readable:
# {"ok": bool, "verdict": "pass|fail|retracted", "checks": [...], "errors": [...]}.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE=""
JSON_OUTPUT=0
RETRACTIONS_ROOT="${FT_ATTESTATION_RETRACTIONS_ROOT:-$REPO_ROOT/docs/attestations/retractions}"

usage() {
  cat <<EOF
Usage: $0 <bundle.json> [--json] [--strict-required] [--strict-deferred]

  --json             Emit machine-readable JSON.
  --strict-required  Fail if the bundle's required_categories list does not match
                     the canonical list in docs/attestations/manifest.json, allowing
                     deferred_slots to satisfy categories in non-release checks.
  --strict-deferred  Fail if the bundle declares any deferred_slots.

Environment:
  FT_ATTESTATION_RETRACTIONS_ROOT
                     Override active retraction root. Defaults to
                     docs/attestations/retractions. The verifier also
                     consults <root>/archive/<bundle-sha>/ on cache misses.
EOF
}

STRICT_REQUIRED=0
STRICT_DEFERRED=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)             JSON_OUTPUT=1; shift ;;
    --strict-required)  STRICT_REQUIRED=1; shift ;;
    --strict-deferred)  STRICT_DEFERRED=1; shift ;;
    -h|--help)          usage; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
    *) [[ -z "$BUNDLE" ]] || { echo "extra arg: $1" >&2; exit 2; }; BUNDLE="$1"; shift ;;
  esac
done

[[ -n "$BUNDLE" ]] || { usage >&2; exit 2; }

if [[ ! -f "$BUNDLE" ]]; then
  # Allow repo-relative.
  if [[ -f "$REPO_ROOT/$BUNDLE" ]]; then BUNDLE="$REPO_ROOT/$BUNDLE"; else
    echo "error: bundle not found: $BUNDLE" >&2; exit 2
  fi
fi

command -v jq >/dev/null 2>&1 || { echo "error: jq is required" >&2; exit 1; }

sha256_file() {
  local f="$1"
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$f" | awk '{print $1}'
  else shasum -a 256 "$f" | awk '{print $1}'; fi
}
sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | awk '{print $1}'
  else shasum -a 256 | awk '{print $1}'; fi
}
is_hex_len() {
  local value="$1" expected_len="$2"
  [[ ${#value} -eq "$expected_len" && ! "$value" =~ [^0-9A-Fa-f] ]]
}

declare -a checks=()
declare -a errors=()
declare -a retraction_results=()
VALID_RETRACTION_COUNT=0
INVALID_RETRACTION_COUNT=0
record_check() {
  # name, ok(true|false), detail
  local name="$1" ok="$2" detail="$3"
  local obj
  obj="$(jq -n --arg name "$name" --argjson ok "$ok" --arg detail "$detail" \
    '{name:$name, ok:$ok, detail:$detail}')"
  checks+=("$obj")
  if [[ "$ok" != "true" ]]; then errors+=("$name: $detail"); fi
}

add_retraction_result() {
  local obj="$1"
  retraction_results+=("$obj")
}

bundle_json="$(cat "$BUNDLE")"

verify_retraction_signature() {
  local retraction_path="$1"
  local retraction_json="$2"
  local affected_slot="$3"
  local canonical_payload
  local computed_canonical_sha
  local declared_canonical_sha
  local sig_method

  canonical_payload="$(jq -S -c 'del(.retraction_signature)' <<<"$retraction_json")"
  computed_canonical_sha="$(printf '%s' "$canonical_payload" | sha256_stdin)"
  declared_canonical_sha="$(jq -r '.retraction_signature.canonical_sha256 // ""' <<<"$retraction_json")"
  if [[ "$computed_canonical_sha" != "$declared_canonical_sha" ]]; then
    record_check "retraction_signature:$affected_slot" false "canonical_sha256 mismatch in $retraction_path"
    return 1
  fi

  sig_method="$(jq -r '.retraction_signature.method // ""' <<<"$retraction_json")"
  case "$sig_method" in
    sigstore-cosign-keyless)
      local sigstore_path sigstore_expected_hash sigstore_expected_size sigstore_abs
      local cert_identity cert_issuer canon_tmp
      sigstore_path="$(jq -r '.retraction_signature.sigstore_bundle.path // ""' <<<"$retraction_json")"
      sigstore_expected_hash="$(jq -r '.retraction_signature.sigstore_bundle.sha256 // ""' <<<"$retraction_json")"
      sigstore_expected_size="$(jq -r '.retraction_signature.sigstore_bundle.size_bytes // ""' <<<"$retraction_json")"
      cert_identity="$(jq -r '.retraction_signature.certificate_identity // ""' <<<"$retraction_json")"
      cert_issuer="$(jq -r '.retraction_signature.certificate_oidc_issuer // ""' <<<"$retraction_json")"
      if [[ -z "$sigstore_path" || "$sigstore_path" == /* ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore_bundle.path must be repo-relative in $retraction_path"
        return 1
      elif ! is_hex_len "$sigstore_expected_hash" 64; then
        record_check "retraction_signature:$affected_slot" false "sigstore_bundle.sha256 must be a 32-byte hex SHA-256 in $retraction_path"
        return 1
      elif [[ ! "$sigstore_expected_size" =~ ^[0-9]+$ || "$sigstore_expected_size" -lt 1 ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore_bundle.size_bytes must be a positive integer in $retraction_path"
        return 1
      elif [[ -z "$cert_identity" || -z "$cert_issuer" ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore certificate_identity and certificate_oidc_issuer are required in $retraction_path"
        return 1
      elif ! command -v cosign >/dev/null 2>&1; then
        record_check "retraction_signature:$affected_slot" false "cosign not installed; cannot verify retraction sigstore bundle"
        return 1
      fi

      sigstore_abs="$REPO_ROOT/$sigstore_path"
      if [[ ! -f "$sigstore_abs" ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore bundle missing: $sigstore_path"
        return 1
      fi
      local sigstore_actual_hash sigstore_actual_size
      sigstore_actual_hash="$(sha256_file "$sigstore_abs")"
      sigstore_actual_size="$(wc -c < "$sigstore_abs" | tr -d ' ')"
      if [[ "$sigstore_actual_hash" != "$sigstore_expected_hash" ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore sha256 mismatch in $retraction_path"
        return 1
      elif [[ "$sigstore_actual_size" != "$sigstore_expected_size" ]]; then
        record_check "retraction_signature:$affected_slot" false "sigstore size mismatch in $retraction_path"
        return 1
      fi

      canon_tmp="$(mktemp)"
      printf '%s' "$canonical_payload" > "$canon_tmp"
      if cosign verify-blob \
          --bundle "$sigstore_abs" \
          --certificate-identity "$cert_identity" \
          --certificate-oidc-issuer "$cert_issuer" \
          "$canon_tmp" >/dev/null 2>&1; then
        record_check "retraction_signature:$affected_slot" true "cosign verify-blob ok (identity=$cert_identity)"
        rm -f "$canon_tmp"
        return 0
      fi
      rm -f "$canon_tmp"
      record_check "retraction_signature:$affected_slot" false "cosign verify-blob failed for $retraction_path"
      return 1
      ;;
    ed25519)
      local signature_path public_key signature_abs signature_hex
      local canon_tmp pubkey_der_tmp sig_tmp key_fingerprint
      signature_path="$(jq -r '.retraction_signature.signature_path // ""' <<<"$retraction_json")"
      public_key="$(jq -r '.retraction_signature.public_key // ""' <<<"$retraction_json")"
      if ! command -v openssl >/dev/null 2>&1; then
        record_check "retraction_signature:$affected_slot" false "openssl not installed; cannot verify ed25519 retraction"
        return 1
      elif ! command -v xxd >/dev/null 2>&1; then
        record_check "retraction_signature:$affected_slot" false "xxd not installed; cannot decode ed25519 retraction"
        return 1
      elif [[ -z "$signature_path" || "$signature_path" == /* ]]; then
        record_check "retraction_signature:$affected_slot" false "ed25519 signature_path must be repo-relative in $retraction_path"
        return 1
      elif ! is_hex_len "$public_key" 64; then
        record_check "retraction_signature:$affected_slot" false "ed25519 public_key must be 32 bytes hex-encoded in $retraction_path"
        return 1
      fi

      signature_abs="$REPO_ROOT/$signature_path"
      if [[ ! -f "$signature_abs" ]]; then
        record_check "retraction_signature:$affected_slot" false "ed25519 signature file missing: $signature_path"
        return 1
      fi
      signature_hex="$(tr -d '[:space:]' < "$signature_abs")"
      if ! is_hex_len "$signature_hex" 128; then
        record_check "retraction_signature:$affected_slot" false "ed25519 signature file must contain a 64-byte hex signature"
        return 1
      fi

      canon_tmp="$(mktemp)"
      pubkey_der_tmp="$(mktemp)"
      sig_tmp="$(mktemp)"
      printf '%s' "$canonical_payload" > "$canon_tmp"
      printf '302a300506032b6570032100%s' "$public_key" | xxd -r -p > "$pubkey_der_tmp"
      printf '%s' "$signature_hex" | xxd -r -p > "$sig_tmp"
      if openssl pkeyutl \
          -verify \
          -rawin \
          -pubin \
          -keyform DER \
          -inkey "$pubkey_der_tmp" \
          -sigfile "$sig_tmp" \
          -in "$canon_tmp" >/dev/null 2>&1; then
        key_fingerprint="$(printf '%s' "$public_key" | sha256_stdin)"
        record_check "retraction_signature:$affected_slot" true "ed25519 verify ok (public_key_sha256=$key_fingerprint)"
        rm -f "$canon_tmp" "$pubkey_der_tmp" "$sig_tmp"
        return 0
      fi
      rm -f "$canon_tmp" "$pubkey_der_tmp" "$sig_tmp"
      record_check "retraction_signature:$affected_slot" false "ed25519 verify failed for $retraction_path"
      return 1
      ;;
    unsigned)
      record_check "retraction_signature:$affected_slot" false "unsigned retractions are rejected: $retraction_path"
      return 1
      ;;
    "")
      record_check "retraction_signature:$affected_slot" false "missing retraction_signature.method in $retraction_path"
      return 1
      ;;
    *)
      record_check "retraction_signature:$affected_slot" false "unknown retraction_signature.method '$sig_method' in $retraction_path"
      return 1
      ;;
  esac
}

scan_retractions_for_bundle() {
  local bundle_sha="$1"
  local found=0
  local valid=0
  local invalid=0
  local roots=(
    "$RETRACTIONS_ROOT/$bundle_sha"
    "$RETRACTIONS_ROOT/archive/$bundle_sha"
  )
  local root retraction_path retraction_json original_sha affected_slot rationale
  local retracted_at retracted_by_release retraction_obj corrected_claim_value

  VALID_RETRACTION_COUNT=0
  INVALID_RETRACTION_COUNT=0
  for root in "${roots[@]}"; do
    [[ -d "$root" ]] || continue
    while IFS= read -r retraction_path; do
      case "$retraction_path" in
        *.canonical.json|*.payload.json) continue ;;
      esac
      found=$((found + 1))
      if ! retraction_json="$(jq -c '.' "$retraction_path" 2>/dev/null)"; then
        record_check "retraction:$retraction_path" false "retraction JSON is unreadable"
        invalid=$((invalid + 1))
        continue
      fi
      original_sha="$(jq -r '.original_bundle_sha256 // ""' <<<"$retraction_json")"
      affected_slot="$(jq -r '.affected_slot // ""' <<<"$retraction_json")"
      rationale="$(jq -r '.retraction_rationale // ""' <<<"$retraction_json")"
      retracted_at="$(jq -r '.retracted_at // ""' <<<"$retraction_json")"
      retracted_by_release="$(jq -r '.retracted_by_release // ""' <<<"$retraction_json")"
      if [[ "$original_sha" != "$bundle_sha" ]]; then
        record_check "retraction:$retraction_path" false "original_bundle_sha256 does not match bundle hash"
        invalid=$((invalid + 1))
        continue
      elif [[ -z "$affected_slot" || -z "$rationale" || -z "$retracted_at" || -z "$retracted_by_release" ]]; then
        record_check "retraction:$retraction_path" false "missing affected_slot/retracted_at/retracted_by_release/retraction_rationale"
        invalid=$((invalid + 1))
        continue
      fi

      if verify_retraction_signature "$retraction_path" "$retraction_json" "$affected_slot"; then
        corrected_claim_value="$(jq -c '.corrected_claim_value // null' <<<"$retraction_json")"
        retraction_obj="$(jq -n \
          --arg path "$retraction_path" \
          --arg original_bundle_sha256 "$original_sha" \
          --arg affected_slot "$affected_slot" \
          --arg retracted_at "$retracted_at" \
          --arg retracted_by_release "$retracted_by_release" \
          --arg retraction_rationale "$rationale" \
          --argjson corrected_claim_value "$corrected_claim_value" \
          '{
            path: $path,
            original_bundle_sha256: $original_bundle_sha256,
            affected_slot: $affected_slot,
            retracted_at: $retracted_at,
            retracted_by_release: $retracted_by_release,
            retraction_rationale: $retraction_rationale,
            corrected_claim_value: $corrected_claim_value
          }')"
        add_retraction_result "$retraction_obj"
        record_check "retraction:$affected_slot" true "valid retraction in $retraction_path"
        valid=$((valid + 1))
      else
        invalid=$((invalid + 1))
      fi
    done < <(find "$root" -type f -name '*.json' -print | sort)
  done

  if [[ ${#retraction_results[@]} -gt 0 ]]; then
    local selected_retractions conflict_count
    selected_retractions="$(
      printf '%s\n' "${retraction_results[@]}" | jq -c -s '
        group_by(.affected_slot)
        | map(
            sort_by(.retracted_at, .path) as $group
            | ($group | last) as $winner
            | if ($group | length) > 1 then
                $winner + {
                  retraction_conflict: {
                    competing_retractions: ($group | length),
                    selected_by: "retracted_at_then_path",
                    superseded_paths: ($group | map(.path) - [$winner.path])
                  }
                }
              else
                $winner
              end
          )
        | sort_by(.affected_slot)
      '
    )"
    conflict_count="$(jq '[.[] | select(has("retraction_conflict"))] | length' <<<"$selected_retractions")"
    retraction_results=()
    while IFS= read -r selected_retraction; do
      retraction_results+=("$selected_retraction")
    done < <(jq -c '.[]' <<<"$selected_retractions")
    if [[ "$conflict_count" -gt 0 ]]; then
      record_check "retraction-conflict" true "$conflict_count affected slot(s) had competing signed retractions; selected latest retracted_at/path"
    fi
  fi

  if [[ "$found" -eq 0 ]]; then
    record_check "retractions" true "none found for bundle sha256 $bundle_sha"
  elif [[ "$valid" -gt 0 && "$invalid" -eq 0 ]]; then
    record_check "retractions" true "$valid valid retraction(s) found for bundle sha256 $bundle_sha"
  elif [[ "$valid" -gt 0 ]]; then
    record_check "retractions" false "$valid valid and $invalid invalid retraction(s) found for bundle sha256 $bundle_sha"
  else
    record_check "retractions" false "$invalid invalid retraction(s) found for bundle sha256 $bundle_sha"
  fi
  VALID_RETRACTION_COUNT="$valid"
  INVALID_RETRACTION_COUNT="$invalid"
}

# Schema-shape sanity (we re-validate against schema.json only structurally; full JSON Schema
# validation would require an external validator. We keep this lightweight on purpose.)
schema_version="$(jq -r '.schema_version // ""' <<<"$bundle_json")"
if [[ "$schema_version" != "1.0.0" ]]; then
  record_check "schema_version" false "expected 1.0.0, got '$schema_version'"
else
  record_check "schema_version" true "1.0.0"
fi

# Required top-level fields.
for field in release generated_at generator git artifacts required_categories taxonomy_coverage confidence_summary signature; do
  has="$(jq -e --arg f "$field" 'has($f)' <<<"$bundle_json" >/dev/null 2>&1 && echo true || echo false)"
  if [[ "$has" == "true" ]]; then record_check "field:$field" true "present"
  else record_check "field:$field" false "missing top-level field"; fi
done

taxonomy_category_count="$(jq '(.taxonomy_coverage.category_counts // []) | length' <<<"$bundle_json")"
taxonomy_below_threshold="$(jq -r '.taxonomy_coverage.below_threshold_count // empty' <<<"$bundle_json")"
taxonomy_uncategorized="$(jq -r '.taxonomy_coverage.uncategorized_artifact_count // empty' <<<"$bundle_json")"
if [[ "$taxonomy_category_count" -lt 10 ]]; then
  record_check "taxonomy_coverage" false "expected at least 10 taxonomy category rows, got $taxonomy_category_count"
elif [[ ! "$taxonomy_below_threshold" =~ ^[0-9]+$ ]]; then
  record_check "taxonomy_coverage" false "taxonomy_coverage.below_threshold_count must be a non-negative integer"
elif [[ ! "$taxonomy_uncategorized" =~ ^[0-9]+$ ]]; then
  record_check "taxonomy_coverage" false "taxonomy_coverage.uncategorized_artifact_count must be a non-negative integer"
else
  record_check "taxonomy_coverage" true "$taxonomy_category_count categories, $taxonomy_below_threshold below threshold, $taxonomy_uncategorized uncategorized artifact(s)"
fi

confidence_schema_path="$(jq -r '.confidence_summary.schema_path // ""' <<<"$bundle_json")"
confidence_record_count="$(jq '(.confidence_summary.records // []) | length' <<<"$bundle_json")"
confidence_best_count="$(jq '(.confidence_summary.best_confidence_by_category // []) | length' <<<"$bundle_json")"
confidence_category_count="$(jq '[.artifacts[]? | (.proof_categories // [])[]] | unique | length' <<<"$bundle_json")"
confidence_bad_records="$(jq '
  [
    ((.confidence_summary.records // []) + (.confidence_summary.best_confidence_by_category // []))[]
    | . as $record
    | select(
        ($record.proof_id | type != "string")
        or ($record.proof_category | type != "number")
        or ((["frequentist", "bayesian", "formal", "topological"] | index($record.confidence_type)) == null)
        or ($record.source_artifact_hash | type != "string")
        or (($record.source_artifact_hash // "") | test("^[0-9a-f]{64}$") | not)
      )
  ] | length
' <<<"$bundle_json")"
if [[ "$confidence_schema_path" != "docs/proofs/confidence-format-schema.json" ]]; then
  record_check "confidence_summary" false "unexpected schema_path: $confidence_schema_path"
elif [[ "$confidence_best_count" -lt "$confidence_category_count" ]]; then
  record_check "confidence_summary" false "expected at least $confidence_category_count best-confidence category row(s), got $confidence_best_count"
elif [[ "$confidence_bad_records" != "0" ]]; then
  record_check "confidence_summary" false "$confidence_bad_records malformed best-confidence record(s)"
else
  record_check "confidence_summary" true "$confidence_record_count record(s), $confidence_best_count best-by-category row(s)"
fi

# Strict required-categories check (optional).
if [[ $STRICT_REQUIRED -eq 1 ]]; then
  manifest_path="$REPO_ROOT/docs/attestations/manifest.json"
  if [[ -f "$manifest_path" ]]; then
    manifest_required="$(jq -c -S '.required_categories | unique' "$manifest_path")"
    bundle_required="$(jq -c -S '((.required_categories // []) + ((.deferred_slots // []) | map(.category))) | unique' <<<"$bundle_json")"
    if [[ "$manifest_required" == "$bundle_required" ]]; then
      record_check "required_categories_match_manifest" true "matches docs/attestations/manifest.json via artifacts + deferred_slots"
    else
      record_check "required_categories_match_manifest" false "diverges from docs/attestations/manifest.json"
    fi
  else
    record_check "required_categories_match_manifest" false "manifest.json missing"
  fi
fi

# Every artifact category in the bundle must be declared by the local
# attestation manifest. This catches forged or stale slots even when the
# canonical payload was recomputed by a broken producer.
manifest_path="$REPO_ROOT/docs/attestations/manifest.json"
if [[ -f "$manifest_path" ]]; then
  manifest_categories="$(jq -c '[.slots[]?.category] | unique' "$manifest_path")"
  unknown_artifact_categories="$(jq -r --argjson allowed "$manifest_categories" '
    [.artifacts[]?.category as $category | $category | select(($allowed | index($category)) | not)]
    | unique
    | join(", ")
  ' <<<"$bundle_json")"
  if [[ -n "$unknown_artifact_categories" ]]; then
    record_check "artifact_categories_declared" false "unknown artifact category/categories: $unknown_artifact_categories"
  else
    record_check "artifact_categories_declared" true "all artifact categories are declared in docs/attestations/manifest.json"
  fi
else
  record_check "artifact_categories_declared" false "manifest.json missing"
fi

# Required-category coverage: every entry in required_categories must have ≥1 artifact.
mapfile -t required_cats < <(jq -r '.required_categories[]?' <<<"$bundle_json")
for cat in "${required_cats[@]}"; do
  count="$(jq --arg c "$cat" '[.artifacts[] | select(.category == $c)] | length' <<<"$bundle_json")"
  if [[ "$count" -ge 1 ]]; then
    record_check "category:$cat" true "$count artifact(s)"
  else
    record_check "category:$cat" false "no artifact found for required category"
  fi
done

# Deferred-slot audit: dev bundles may expose intentionally missing
# manifest slots. They are not artifact categories, but they must be
# visible and release gates can reject them with --strict-deferred.
deferred_count="$(jq '(.deferred_slots // []) | length' <<<"$bundle_json")"
for ((i=0; i<deferred_count; i++)); do
  def="$(jq -c "(.deferred_slots // [])[$i]" <<<"$bundle_json")"
  category="$(jq -r '.category // ""' <<<"$def")"
  deferred_to_bead="$(jq -r '.deferred_to_bead // ""' <<<"$def")"
  reason="$(jq -r '.deferred_reason // ""' <<<"$def")"
  if [[ -z "$category" || -z "$deferred_to_bead" || -z "$reason" ]]; then
    record_check "deferred_slot:$i" false "missing category/deferred_to_bead/deferred_reason"
    continue
  fi
  if [[ ! "$deferred_to_bead" =~ ^ft-[a-z0-9.]+$ ]]; then
    record_check "deferred_slot:$category" false "invalid deferred_to_bead: $deferred_to_bead"
    continue
  fi
  record_check "deferred_slot:$category" true "deferred to $deferred_to_bead"
done
if [[ "$deferred_count" -gt 0 && "$STRICT_DEFERRED" -eq 1 ]]; then
  record_check "deferred_slots_strict" false "--strict-deferred rejects $deferred_count deferred slot(s)"
elif [[ "$deferred_count" -gt 0 ]]; then
  record_check "deferred_slots" true "$deferred_count deferred slot(s) declared"
else
  record_check "deferred_slots" true "none"
fi

# Per-artifact hash recomputation.
artifact_count="$(jq '.artifacts | length' <<<"$bundle_json")"
for ((i=0; i<artifact_count; i++)); do
  art="$(jq -c ".artifacts[$i]" <<<"$bundle_json")"
  path="$(jq -r '.path' <<<"$art")"
  expected="$(jq -r '.sha256' <<<"$art")"
  expected_size="$(jq -r '.size_bytes' <<<"$art")"
  abs="$REPO_ROOT/$path"
  if [[ ! -f "$abs" ]]; then
    record_check "artifact:$path" false "file missing on disk"
    continue
  fi
  actual="$(sha256_file "$abs")"
  actual_size="$(wc -c < "$abs" | tr -d ' ')"
  if [[ "$actual" != "$expected" ]]; then
    record_check "artifact:$path" false "sha256 mismatch (expected $expected, got $actual)"
  elif [[ "$actual_size" != "$expected_size" ]]; then
    record_check "artifact:$path" false "size mismatch (expected $expected_size, got $actual_size)"
  else
    record_check "artifact:$path" true "sha256 + size ok"
  fi
done

# Canonical-payload recomputation.
canonical_payload="$(jq -S -c 'del(.signature)' <<<"$bundle_json")"
recomputed_canonical_sha="$(printf '%s' "$canonical_payload" | sha256_stdin)"
declared_canonical_sha="$(jq -r '.signature.canonical_sha256 // ""' <<<"$bundle_json")"
if [[ "$recomputed_canonical_sha" == "$declared_canonical_sha" ]]; then
  record_check "canonical_sha256" true "$recomputed_canonical_sha"
else
  record_check "canonical_sha256" false "expected $declared_canonical_sha, got $recomputed_canonical_sha"
fi

# br-ft-q0tz3 / BR-RC-RUNTIME-SEMANTICS.G14.2: semantic check on
# the doctrine/cx-propagation snapshot. The hash check above
# proves the file's bytes match the bundle's recorded sha256;
# this check additionally proves the snapshot's content
# satisfies the bead's release-gate rule (totals.uncovered_sites
# == 0). A bundle that ships a parseable snapshot with non-zero
# uncovered Cx-propagation sites must fail.
cx_artifact_path="$(jq -r '[.artifacts[] | select(.category == "doctrine/cx-propagation")][0].path // ""' <<<"$bundle_json")"
if [[ -z "$cx_artifact_path" ]]; then
  # Only assert presence when the bundle commits to the category.
  if jq -e '.required_categories | index("doctrine/cx-propagation")' <<<"$bundle_json" >/dev/null 2>&1; then
    record_check "doctrine/cx-propagation" false "bundle committed to category but no artifact found"
  fi
else
  cx_abs="$REPO_ROOT/$cx_artifact_path"
  if [[ ! -f "$cx_abs" ]]; then
    record_check "doctrine/cx-propagation" false "snapshot file missing: $cx_artifact_path"
  elif ! cx_payload="$(jq '.' "$cx_abs" 2>/dev/null)"; then
    record_check "doctrine/cx-propagation" false "snapshot is not valid JSON: $cx_artifact_path"
  else
    uncov="$(jq -r '.totals.uncovered_sites // empty' <<<"$cx_payload")"
    if [[ -z "$uncov" ]]; then
      record_check "doctrine/cx-propagation" false "snapshot lacks .totals.uncovered_sites field"
    elif [[ "$uncov" == "0" ]]; then
      cov="$(jq -r '.totals.covered_sites // 0' <<<"$cx_payload")"
      total="$(jq -r '.totals.total_sites // 0' <<<"$cx_payload")"
      record_check "doctrine/cx-propagation" true "totals.uncovered_sites=0 (covered=$cov / total=$total)"
    else
      record_check "doctrine/cx-propagation" false "totals.uncovered_sites=$uncov (must be 0 for release gate)"
    fi
  fi
fi

# Signature verification.
sig_method="$(jq -r '.signature.method // ""' <<<"$bundle_json")"
case "$sig_method" in
  sigstore-cosign-keyless)
    sigstore_path="$(jq -r '.signature.sigstore_bundle.path // ""' <<<"$bundle_json")"
    sigstore_expected_hash="$(jq -r '.signature.sigstore_bundle.sha256 // ""' <<<"$bundle_json")"
    sigstore_expected_size="$(jq -r '.signature.sigstore_bundle.size_bytes // ""' <<<"$bundle_json")"
    sigstore_ok=0
    if [[ -z "$sigstore_path" || "$sigstore_path" == /* ]]; then
      record_check "sigstore_bundle" false "signature.sigstore_bundle.path must be repo-relative"
    elif ! is_hex_len "$sigstore_expected_hash" 64; then
      record_check "sigstore_bundle" false "signature.sigstore_bundle.sha256 must be a 32-byte hex SHA-256"
    elif [[ ! "$sigstore_expected_size" =~ ^[0-9]+$ || "$sigstore_expected_size" -lt 1 ]]; then
      record_check "sigstore_bundle" false "signature.sigstore_bundle.size_bytes must be a positive integer"
    else
      sigstore_abs="$REPO_ROOT/$sigstore_path"
      if [[ ! -f "$sigstore_abs" ]]; then
        record_check "sigstore_bundle" false "sigstore bundle missing: $sigstore_path"
      else
        sigstore_actual_hash="$(sha256_file "$sigstore_abs")"
        sigstore_actual_size="$(wc -c < "$sigstore_abs" | tr -d ' ')"
        if [[ "$sigstore_actual_hash" != "$sigstore_expected_hash" ]]; then
          record_check "sigstore_bundle" false "sha256 mismatch (expected $sigstore_expected_hash, got $sigstore_actual_hash)"
        elif [[ "$sigstore_actual_size" != "$sigstore_expected_size" ]]; then
          record_check "sigstore_bundle" false "size mismatch (expected $sigstore_expected_size, got $sigstore_actual_size)"
        else
          record_check "sigstore_bundle" true "sha256 + size ok"
          sigstore_ok=1
        fi
      fi
    fi

    cert_identity="$(jq -r '.signature.certificate_identity // ""' <<<"$bundle_json")"
    cert_issuer="$(jq -r '.signature.certificate_oidc_issuer // ""' <<<"$bundle_json")"
    if [[ -z "$cert_identity" || -z "$cert_issuer" ]]; then
      record_check "signature" false "sigstore certificate_identity and certificate_oidc_issuer are required"
    elif [[ "$sigstore_ok" != "1" ]]; then
      record_check "signature" false "sigstore bundle metadata did not validate"
    elif ! command -v cosign >/dev/null 2>&1; then
      record_check "signature" false "cosign not installed; cannot verify sigstore-cosign-keyless"
    else
      sigstore_abs="$REPO_ROOT/$sigstore_path"
      canon_tmp="$(mktemp)"
      printf '%s' "$canonical_payload" > "$canon_tmp"
      if cosign verify-blob \
          --bundle "$sigstore_abs" \
          --certificate-identity "$cert_identity" \
          --certificate-oidc-issuer "$cert_issuer" \
          "$canon_tmp" >/dev/null 2>&1; then
        record_check "signature" true "cosign verify-blob ok (identity=$cert_identity)"
      else
        record_check "signature" false "cosign verify-blob failed (identity=$cert_identity, issuer=$cert_issuer)"
      fi
      rm -f "$canon_tmp"
    fi
    ;;
  ed25519)
    if ! command -v openssl >/dev/null 2>&1; then
      record_check "signature" false "openssl not installed; cannot verify ed25519"
    elif ! command -v xxd >/dev/null 2>&1; then
      record_check "signature" false "xxd not installed; cannot decode ed25519 hex material"
    else
      signature_path="$(jq -r '.signature.signature_path // ""' <<<"$bundle_json")"
      public_key="$(jq -r '.signature.public_key // ""' <<<"$bundle_json")"
      if [[ -z "$signature_path" || "$signature_path" == /* ]]; then
        record_check "signature" false "ed25519 signature_path must be a repo-relative path"
      elif ! is_hex_len "$public_key" 64; then
        record_check "signature" false "ed25519 public_key must be 32 bytes hex-encoded"
      else
        signature_abs="$REPO_ROOT/$signature_path"
        if [[ ! -f "$signature_abs" ]]; then
          record_check "signature" false "ed25519 signature file missing: $signature_path"
        else
          signature_hex="$(tr -d '[:space:]' < "$signature_abs")"
          if ! is_hex_len "$signature_hex" 128; then
            record_check "signature" false "ed25519 signature file must contain a 64-byte hex signature"
          else
            canon_tmp="$(mktemp)"
            pubkey_der_tmp="$(mktemp)"
            sig_tmp="$(mktemp)"
            printf '%s' "$canonical_payload" > "$canon_tmp"
            # SubjectPublicKeyInfo DER prefix for Ed25519 (RFC 8410) followed by
            # the raw 32-byte public key from the attestation schema.
            printf '302a300506032b6570032100%s' "$public_key" | xxd -r -p > "$pubkey_der_tmp"
            printf '%s' "$signature_hex" | xxd -r -p > "$sig_tmp"
            if openssl pkeyutl \
                -verify \
                -rawin \
                -pubin \
                -keyform DER \
                -inkey "$pubkey_der_tmp" \
                -sigfile "$sig_tmp" \
                -in "$canon_tmp" >/dev/null 2>&1; then
              key_fingerprint="$(printf '%s' "$public_key" | sha256_stdin)"
              record_check "signature" true "ed25519 verify ok (public_key_sha256=$key_fingerprint)"
            else
              record_check "signature" false "ed25519 verify failed (signature_path=$signature_path)"
            fi
            rm -f "$canon_tmp" "$pubkey_der_tmp" "$sig_tmp"
          fi
        fi
      fi
    fi
    ;;
  unsigned)
    reason="$(jq -r '.signature.reason // ""' <<<"$bundle_json")"
    record_check "signature" true "unsigned (reason: $reason)"
    ;;
  "")
    record_check "signature" false "no signature.method declared"
    ;;
  *)
    record_check "signature" false "unknown signature.method: $sig_method"
    ;;
esac

bundle_file_sha256="$(sha256_file "$BUNDLE")"
scan_retractions_for_bundle "$bundle_file_sha256"

# Aggregate.
ok=true
for c in "${checks[@]}"; do
  cok="$(jq -r '.ok' <<<"$c")"
  [[ "$cok" == "true" ]] || ok=false
done

verdict="fail"
output_ok=false
exit_code=1
if [[ "$VALID_RETRACTION_COUNT" -gt 0 && "$INVALID_RETRACTION_COUNT" -eq 0 ]]; then
  verdict="retracted"
  output_ok=false
  exit_code=3
elif [[ "$ok" == "true" ]]; then
  verdict="pass"
  output_ok=true
  exit_code=0
fi

if [[ $JSON_OUTPUT -eq 1 ]]; then
  checks_arr="$(printf '%s\n' "${checks[@]}" | jq -c -s '.')"
  errors_arr="$(printf '%s\n' "${errors[@]}" | jq -R -c -s 'split("\n") | map(select(. != ""))')"
  if [[ ${#retraction_results[@]} -eq 0 ]]; then
    retractions_arr="[]"
  else
    retractions_arr="$(printf '%s\n' "${retraction_results[@]}" | jq -c -s '.')"
  fi
  jq -n \
    --argjson ok "$output_ok" \
    --arg verdict "$verdict" \
    --arg bundle "$BUNDLE" \
    --arg bundle_sha256 "$bundle_file_sha256" \
    --argjson checks "$checks_arr" \
    --argjson errors "$errors_arr" \
    --argjson retractions "$retractions_arr" \
    '{ok:$ok, verdict:$verdict, bundle:$bundle, bundle_sha256:$bundle_sha256, checks:$checks, errors:$errors, retractions:$retractions}'
else
  for c in "${checks[@]}"; do
    cok="$(jq -r '.ok' <<<"$c")"
    cname="$(jq -r '.name' <<<"$c")"
    cdet="$(jq -r '.detail' <<<"$c")"
    if [[ "$cok" == "true" ]]; then printf "  PASS  %-40s %s\n" "$cname" "$cdet"
    else                            printf "  FAIL  %-40s %s\n" "$cname" "$cdet"; fi
  done
  if [[ "$verdict" == "pass" ]]; then
    echo "OK: $BUNDLE"
  elif [[ "$verdict" == "retracted" ]]; then
    echo "RETRACTED: $BUNDLE (${#retraction_results[@]} retraction(s))"
    for r in "${retraction_results[@]}"; do
      slot="$(jq -r '.affected_slot' <<<"$r")"
      rationale="$(jq -r '.retraction_rationale' <<<"$r")"
      corrected="$(jq -c '.corrected_claim_value' <<<"$r")"
      echo "  - $slot: $rationale"
      if [[ "$corrected" != "null" ]]; then
        echo "    corrected_claim_value: $corrected"
      fi
    done
  else
    echo "FAIL: $BUNDLE (${#errors[@]} error(s))"
  fi
fi

exit "$exit_code"
