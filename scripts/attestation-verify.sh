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
Usage: $0 <bundle.json> [--json] [--strict-required]

  --json             Emit machine-readable JSON.
  --strict-required  Fail if the bundle's required_categories list does not match
                     the canonical list in docs/attestations/manifest.json.
EOF
}

STRICT_REQUIRED=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)             JSON_OUTPUT=1; shift ;;
    --strict-required)  STRICT_REQUIRED=1; shift ;;
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
    manifest_required="$(jq -c -S '.required_categories' "$manifest_path")"
    bundle_required="$(jq -c -S '.required_categories' <<<"$bundle_json")"
    if [[ "$manifest_required" == "$bundle_required" ]]; then
      record_check "required_categories_match_manifest" true "matches docs/attestations/manifest.json"
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
    if ! command -v cosign >/dev/null 2>&1; then
      record_check "signature" false "cosign not installed; cannot verify sigstore-cosign-keyless"
    else
      sigstore_path="$(jq -r '.signature.bundle_path' <<<"$bundle_json")"
      cert_identity="$(jq -r '.signature.certificate_identity' <<<"$bundle_json")"
      cert_issuer="$(jq -r '.signature.certificate_oidc_issuer' <<<"$bundle_json")"
      sigstore_abs="$REPO_ROOT/$sigstore_path"
      if [[ ! -f "$sigstore_abs" ]]; then
        record_check "signature" false "sigstore bundle missing: $sigstore_path"
      else
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
    fi
    ;;
  ed25519)
    record_check "signature" false "ed25519 verification not yet implemented (schema reserves it; ft-syqcz.1.1 will land it)"
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
