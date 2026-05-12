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
# Exit code: 0 on full pass, 1 on any failure, 2 on usage error.
# JSON output (with --json) is machine-readable: {"ok": bool, "checks": [...], "errors": [...]}.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE=""
JSON_OUTPUT=0

usage() {
  cat <<EOF
Usage: $0 <bundle.json> [--json] [--strict-required] [--strict-deferred]

  --json             Emit machine-readable JSON.
  --strict-required  Fail if the bundle's required_categories list does not match
                     the canonical list in docs/attestations/manifest.json, allowing
                     deferred_slots to satisfy categories in non-release checks.
  --strict-deferred  Fail if the bundle declares any deferred_slots.
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
record_check() {
  # name, ok(true|false), detail
  local name="$1" ok="$2" detail="$3"
  local obj
  obj="$(jq -n --arg name "$name" --argjson ok "$ok" --arg detail "$detail" \
    '{name:$name, ok:$ok, detail:$detail}')"
  checks+=("$obj")
  if [[ "$ok" != "true" ]]; then errors+=("$name: $detail"); fi
}

bundle_json="$(cat "$BUNDLE")"

# Schema-shape sanity (we re-validate against schema.json only structurally; full JSON Schema
# validation would require an external validator. We keep this lightweight on purpose.)
schema_version="$(jq -r '.schema_version // ""' <<<"$bundle_json")"
if [[ "$schema_version" != "1.0.0" ]]; then
  record_check "schema_version" false "expected 1.0.0, got '$schema_version'"
else
  record_check "schema_version" true "1.0.0"
fi

# Required top-level fields.
for field in release generated_at generator git artifacts required_categories signature; do
  has="$(jq -e --arg f "$field" 'has($f)' <<<"$bundle_json" >/dev/null 2>&1 && echo true || echo false)"
  if [[ "$has" == "true" ]]; then record_check "field:$field" true "present"
  else record_check "field:$field" false "missing top-level field"; fi
done

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

# Aggregate.
ok=true
for c in "${checks[@]}"; do
  cok="$(jq -r '.ok' <<<"$c")"
  [[ "$cok" == "true" ]] || ok=false
done

if [[ $JSON_OUTPUT -eq 1 ]]; then
  checks_arr="$(printf '%s\n' "${checks[@]}" | jq -c -s '.')"
  errors_arr="$(printf '%s\n' "${errors[@]}" | jq -R -c -s 'split("\n") | map(select(. != ""))')"
  jq -n \
    --argjson ok "$ok" \
    --arg bundle "$BUNDLE" \
    --argjson checks "$checks_arr" \
    --argjson errors "$errors_arr" \
    '{ok:$ok, bundle:$bundle, checks:$checks, errors:$errors}'
else
  for c in "${checks[@]}"; do
    cok="$(jq -r '.ok' <<<"$c")"
    cname="$(jq -r '.name' <<<"$c")"
    cdet="$(jq -r '.detail' <<<"$c")"
    if [[ "$cok" == "true" ]]; then printf "  PASS  %-40s %s\n" "$cname" "$cdet"
    else                            printf "  FAIL  %-40s %s\n" "$cname" "$cdet"; fi
  done
  if [[ "$ok" == "true" ]]; then echo "OK: $BUNDLE"
  else echo "FAIL: $BUNDLE (${#errors[@]} error(s))"; fi
fi

[[ "$ok" == "true" ]] || exit 1
