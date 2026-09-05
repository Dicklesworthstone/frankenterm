#!/usr/bin/env bash
# tests/attestation/smoke-test.sh — round-trip test for attestation-build.sh / attestation-verify.sh.
#
# Builds a synthetic bundle from a temp manifest pointing at real files,
# verifies it, tampers with one artifact, and re-verifies (expects failure).
#
# Defined by ft-syqcz.1 (BR-RC-FOUNDATION.G3.1). Run with: bash tests/attestation/smoke-test.sh

set -euo pipefail
umask 077
# Test inputs are owned fixtures; never inherit an operator's production trust.
unset FT_ATTESTATION_RELEASE_POLICY FT_ATTESTATION_MANIFEST

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

RUN_ID="${FT_ATTESTATION_SMOKE_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
WORKDIR="${FT_ATTESTATION_SMOKE_DIR:-$REPO_ROOT/target/test-artifacts/attestation-smoke/${RUN_ID}}"
if [[ -d "$WORKDIR" && -n "$(ls -A "$WORKDIR")" ]]; then
  echo "refusing to overwrite nonempty smoke evidence: $WORKDIR" >&2
  exit 2
fi
OUT_DIR="$WORKDIR/attestations"
mkdir -p "$OUT_DIR"

for invalid_version in '../escaped-version' '0.0.0/escaped-version'; do
  if FT_ATTESTATION_OUT_DIR="$OUT_DIR" bash scripts/attestation-build.sh \
    --version "$invalid_version" --channel dev --sign unsigned \
    >"$WORKDIR/invalid-version.out" 2>&1; then
    echo "FAIL: build accepted a version containing path components" >&2
    exit 1
  fi
  grep -q 'version must be a semver' "$WORKDIR/invalid-version.out"
done

# Stage two real files as fake "artifacts" — the schema and the manifest themselves.
# These are stable, never empty, and live in-repo.
ART_DIR="$REPO_ROOT/docs/attestations"

# Build a temp manifest that overrides only one slot.
TMP_MANIFEST="$WORKDIR/manifest.json"
cp "$ART_DIR/manifest.json" "$TMP_MANIFEST"
jq '(.slots[] | select(.category == "doctrine/agents-md-counts")).path = "docs/attestations/schema.json"' \
   "$TMP_MANIFEST" > "$TMP_MANIFEST.new" && mv "$TMP_MANIFEST.new" "$TMP_MANIFEST"

echo "=== positive build ==="
FT_ATTESTATION_MANIFEST="$TMP_MANIFEST" \
FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
  bash scripts/attestation-build.sh --version 0.0.0-smoke --channel dev --sign unsigned --allow-partial >"$WORKDIR/build.out" 2>&1
cat "$WORKDIR/build.out"
BUNDLE="$OUT_DIR/0.0.0-smoke.json"

echo
echo "=== positive verify ==="
bash scripts/attestation-verify.sh "$BUNDLE" >"$WORKDIR/verify.out"
cat "$WORKDIR/verify.out"

# Sanity: the bundle must include exactly one doctrine/agents-md-counts artifact.
count="$(jq '[.artifacts[] | select(.category == "doctrine/agents-md-counts")] | length' \
        "$BUNDLE")"
if [[ "$count" != "1" ]]; then
  echo "FAIL: expected 1 doctrine/agents-md-counts artifact, got $count"
  exit 1
fi
echo
echo "=== positive verify: artifact count OK ==="

if command -v openssl >/dev/null 2>&1 && command -v xxd >/dev/null 2>&1; then
  ED_KEY="$WORKDIR/ed25519.pem"
  ED_BUILD_VERSION="0.0.0-smoke-ed25519"
  ED_BUNDLE="$OUT_DIR/${ED_BUILD_VERSION}.json"
  ED_BAD_BUNDLE="$WORKDIR/ed25519-bad-bundle.json"
  ED_BAD_SIG="$WORKDIR/${ED_BUILD_VERSION}.bad-ed25519.sig.hex"
  ED_BAD_SIG_REL="${ED_BAD_SIG#"$REPO_ROOT"/}"

  openssl genpkey -algorithm ED25519 -out "$ED_KEY"
  ED25519_PRIVATE_KEY_PATH="$ED_KEY" \
  FT_ATTESTATION_MANIFEST="$TMP_MANIFEST" \
  FT_ATTESTATION_OUT_DIR="$OUT_DIR" \
    bash scripts/attestation-build.sh \
      --version "$ED_BUILD_VERSION" \
      --channel dev \
      --sign ed25519 \
      --allow-partial >"$WORKDIR/ed25519_build.out" 2>&1
  cat "$WORKDIR/ed25519_build.out"

  echo
  echo "=== ed25519 build + verify ==="
  bash scripts/attestation-verify.sh "$ED_BUNDLE" >"$WORKDIR/ed25519.out"
  cat "$WORKDIR/ed25519.out"

  ED_SIG_REL="$(jq -r '.signature.signature_path' "$ED_BUNDLE")"
  cp "$REPO_ROOT/$ED_SIG_REL" "$ED_BAD_SIG"
  # Flip the first byte to a different valid hex value while preserving length.
  sig_value="$(tr -d '[:space:]' < "$ED_BAD_SIG")"
  replacement="00"
  if [[ "${sig_value:0:2}" == "00" ]]; then
    replacement="ff"
  fi
  printf '%s%s\n' "$replacement" "${sig_value:2}" > "$ED_BAD_SIG"
  jq --arg sig_path "$ED_BAD_SIG_REL" '.signature.signature_path = $sig_path' \
    "$ED_BUNDLE" > "$ED_BAD_BUNDLE"

  echo
  echo "=== ed25519 bad-signature test (expect verify to FAIL) ==="
  if bash scripts/attestation-verify.sh "$ED_BAD_BUNDLE" >"$WORKDIR/ed25519_bad.out" 2>&1; then
    echo "FAIL: bad ed25519 signature verified successfully"
    cat "$WORKDIR/ed25519_bad.out"
    exit 1
  fi
  grep -q "ed25519 verify failed" "$WORKDIR/ed25519_bad.out" || {
    echo "FAIL: ed25519 verifier did not report signature failure"
    cat "$WORKDIR/ed25519_bad.out"
    exit 1
  }
  cat "$WORKDIR/ed25519_bad.out"

  echo
  echo "=== external release trust: real Ed25519 signatures over owned fixture artifacts ==="
  # This complete manifest is a verifier fixture, not production proof. The
  # external policy comes from this test's independently generated trust root.
  # Neither the production manifest nor any production key is modified.
  RELEASE_MANIFEST="$WORKDIR/release-manifest.json"
  RELEASE_RECEIPT="$WORKDIR/fixture-receipt.json"
  printf '{"fixture_only":true,"totals":{"uncovered_sites":0,"covered_sites":1,"total_sites":1}}\n' >"$RELEASE_RECEIPT"
  jq --arg path "${RELEASE_RECEIPT#"$REPO_ROOT"/}" '
    .slots |= (map(.path = $path | del(.deferred_to_bead, .deferred_reason)) | unique_by(.category))
  ' "$ART_DIR/manifest.json" >"$RELEASE_MANIFEST"
  RELEASE_VERSION=0.0.0-release-trust-fixture
  RELEASE_BUNDLE="$OUT_DIR/$RELEASE_VERSION.json"
  ED25519_PRIVATE_KEY_PATH="$ED_KEY" FT_ATTESTATION_MANIFEST="$RELEASE_MANIFEST" \
    FT_ATTESTATION_OUT_DIR="$OUT_DIR" bash scripts/attestation-build.sh \
      --version "$RELEASE_VERSION" --channel beta --sign ed25519 \
      --profile release-interactive --target aarch64-apple-darwin \
      >"$WORKDIR/release-build.log" 2>&1
  cat "$WORKDIR/release-build.log"
  hash_file() { shasum -a 256 "$1" | awk '{print $1}'; }
  RELEASE_POLICY="$WORKDIR/operator-policy.json"
  now_epoch="$(date -u +%s)"
  jq --arg manifest_sha256 "$(hash_file "$RELEASE_MANIFEST")" --argjson now "$now_epoch" '{
    schema_version:"frankenterm.release-attestation-policy.v1",
    signer:{method:"ed25519",public_key:.signature.public_key,revoked:false,
      not_before:($now-3600),not_after:($now+3600)},
    max_age_seconds:600,release:.release,git:{commit:.git.commit,tree:.git.tree},
    build:.build,manifest_sha256:$manifest_sha256
  }' "$RELEASE_BUNDLE" >"$RELEASE_POLICY"
  # The key is derived independently from our owned private key, not accepted
  # merely because the bundle repeats it.
  openssl pkey -in "$ED_KEY" -pubout -outform DER >"$WORKDIR/trusted-key.der"
  trusted_key="$(tail -c 32 "$WORKDIR/trusted-key.der" | xxd -p -c 256)"
  test "$(jq -r '.signer.public_key' "$RELEASE_POLICY")" = "$trusted_key"

  release_verify() {
    FT_ATTESTATION_MANIFEST="$RELEASE_MANIFEST" bash scripts/attestation-verify.sh \
      "$1" --json --release-policy "$2"
  }
  release_verify "$RELEASE_BUNDLE" "$RELEASE_POLICY" >"$WORKDIR/release-positive.json"
  jq -e '.ok and .publisher_authenticated and .verification_mode == "release_policy"' \
    "$WORKDIR/release-positive.json"
  FT_ATTESTATION_MANIFEST="$RELEASE_MANIFEST" bash scripts/attestation-verify.sh \
    "$RELEASE_BUNDLE" --json >"$WORKDIR/development-signed.json"
  jq -e '.ok and (.publisher_authenticated | not) and .verification_mode == "development_integrity"' \
    "$WORKDIR/development-signed.json"

  sign_fixture() {
    local payload="$1" key="$2" name="$3"
    local canonical="$WORKDIR/$name.canonical.json" signature="$WORKDIR/$name.signature.bin"
    local signature_hex="$WORKDIR/$name.signature.hex" public_der="$WORKDIR/$name.public.der"
    jq -S -c 'del(.signature)' "$payload" | tr -d '\n' >"$canonical"
    openssl pkey -in "$key" -pubout -outform DER >"$public_der"
    openssl pkeyutl -sign -rawin -inkey "$key" -in "$canonical" -out "$signature"
    xxd -p -c 256 "$signature" >"$signature_hex"
    jq --arg key "$(tail -c 32 "$public_der" | xxd -p -c 256)" \
      --arg path "${signature_hex#"$REPO_ROOT"/}" --arg sha "$(hash_file "$canonical")" \
      '.signature = {method:"ed25519",public_key:$key,signature_path:$path,canonical_sha256:$sha}' \
      "$payload" >"$WORKDIR/$name.json"
  }
  expect_release_failure() {
    local name="$1" bundle="$2" policy="$3" check="$4"
    if release_verify "$bundle" "$policy" >"$WORKDIR/$name.verdict.json" 2>"$WORKDIR/$name.stderr"; then
      echo "FAIL: release verifier accepted $name" >&2; exit 1
    fi
    cat "$WORKDIR/$name.stderr"
    jq -e --arg check "$check" '
      (.ok | not) and (.publisher_authenticated | not)
      and any(.checks[]; .name == $check and (.ok | not))
    ' "$WORKDIR/$name.verdict.json" >/dev/null
    printf 'RELEASE_TRUST_NEGATIVE %s rejected_by=%s\n' "$name" "$check"
  }

  OTHER_KEY="$WORKDIR/untrusted-key.pem"
  openssl genpkey -algorithm ED25519 -out "$OTHER_KEY"
  sign_fixture "$RELEASE_BUNDLE" "$OTHER_KEY" unknown-signer
  # An equally valid self-signature passes development integrity, then fails
  # only the independently pinned publisher check.
  FT_ATTESTATION_MANIFEST="$RELEASE_MANIFEST" bash scripts/attestation-verify.sh \
    "$WORKDIR/unknown-signer.json" --json >"$WORKDIR/unknown-signer-dev.json"
  jq -e '.ok and (.publisher_authenticated | not)' "$WORKDIR/unknown-signer-dev.json"
  expect_release_failure unknown-signer "$WORKDIR/unknown-signer.json" "$RELEASE_POLICY" release_signer

  for mutation in wrong-release wrong-source wrong-tree wrong-profile wrong-target stale future unsigned missing-artifact tampered-artifact missing-slot duplicate-slot deferred; do
    case "$mutation" in
      wrong-release) filter='.release.version = "9.9.9" | .release.tag = "v9.9.9"' ;;
      wrong-source) filter='.git.commit = ("a" * 40)' ;;
      wrong-tree) filter='.git.tree = ("b" * 40)' ;;
      wrong-profile) filter='.build.profile = "release-abort-probe"' ;;
      wrong-target) filter='.build.targets = ["x86_64-unknown-linux-gnu"]' ;;
      stale) filter='.generated_at = "2000-01-01T00:00:00Z"' ;;
      future) filter='.generated_at = "2099-01-01T00:00:00Z"' ;;
      unsigned) filter='.signature = {method:"unsigned",reason:"owned fixture"}' ;;
      missing-artifact) filter='.artifacts[0].path = "owned-fixture-does-not-exist.json"' ;;
      tampered-artifact) filter='.artifacts[0].sha256 = ("0" * 64)' ;;
      missing-slot) filter='.artifacts = .artifacts[1:]' ;;
      duplicate-slot) filter='.artifacts += [.artifacts[0]]' ;;
      deferred) filter='.deferred_slots = [{category:.required_categories[0],deferred_to_bead:"ft-fixture",deferred_reason:"owned negative"}]' ;;
    esac
    jq "$filter" "$RELEASE_BUNDLE" >"$WORKDIR/$mutation.payload.json"
    sign_fixture "$WORKDIR/$mutation.payload.json" "$ED_KEY" "$mutation"
    check=release_binding
    case "$mutation" in
      stale|future) check=release_freshness ;;
      unsigned)
        jq '.signature.method = "unsigned"' "$WORKDIR/$mutation.json" >"$WORKDIR/$mutation.final.json"
        cp "$WORKDIR/$mutation.final.json" "$WORKDIR/$mutation.json"
        check=release_signer ;;
      missing-artifact|missing-slot|duplicate-slot) check=release_required_slots ;;
      tampered-artifact) check="artifact:${RELEASE_RECEIPT#"$REPO_ROOT"/}" ;;
      deferred) check=deferred_slots_strict ;;
    esac
    expect_release_failure "$mutation" "$WORKDIR/$mutation.json" "$RELEASE_POLICY" "$check"
  done
  for mutation in revoked expired not-yet-valid malformed-key malformed-version wrong-manifest; do
    case "$mutation" in
      revoked) filter='.signer.revoked = true' ;;
      expired) filter='.signer.not_after = .signer.not_before + 1' ;;
      not-yet-valid) filter='.signer.not_before = .signer.not_after - 1' ;;
      malformed-key) filter='.signer.public_key = "not-a-key"' ;;
      malformed-version) filter='.release.version = "../escaped-version"' ;;
      wrong-manifest) filter='.manifest_sha256 = ("0" * 64)' ;;
    esac
    jq "$filter" "$RELEASE_POLICY" >"$WORKDIR/$mutation.policy.json"
    check=release_signer_validity
    case "$mutation" in
      malformed-key|malformed-version) check=release_policy ;;
      wrong-manifest) check=release_manifest_binding ;;
    esac
    expect_release_failure "$mutation" "$RELEASE_BUNDLE" "$WORKDIR/$mutation.policy.json" "$check"
  done
  expect_release_failure missing-policy "$RELEASE_BUNDLE" "$WORKDIR/absent-policy.json" release_policy
  echo "PASS: external release trust fixtures (no production release or target execution claimed)"
else
  echo
  echo "FAIL: openssl and xxd are required for real signature verification" >&2
  exit 1
fi

# Tamper test: corrupt the recorded sha256 in the bundle, expect verify to fail.
TAMPERED="$WORKDIR/tampered.json"
jq '(.artifacts[0].sha256) = "0000000000000000000000000000000000000000000000000000000000000000"' \
   "$BUNDLE" > "$TAMPERED"
echo
echo "=== tamper test (expect verify to FAIL) ==="
if bash scripts/attestation-verify.sh "$TAMPERED" >"$WORKDIR/tamper.out" 2>&1; then
  echo "FAIL: tampered bundle verified successfully — verify is broken"
  cat "$WORKDIR/tamper.out"
  exit 1
fi
cat "$WORKDIR/tamper.out"
echo
echo "=== tamper detected — verify failed loudly as expected ==="

# Canonical-payload tamper: change a top-level field, expect canonical_sha256 mismatch.
TAMPERED_CANON="$WORKDIR/tampered_canon.json"
jq '.release.channel = "stable"' "$BUNDLE" > "$TAMPERED_CANON"
echo
echo "=== canonical tamper test (expect verify to FAIL on canonical_sha256) ==="
if bash scripts/attestation-verify.sh "$TAMPERED_CANON" >"$WORKDIR/canon.out" 2>&1; then
  echo "FAIL: canonical-tampered bundle verified successfully"
  cat "$WORKDIR/canon.out"
  exit 1
fi
grep -q "canonical_sha256" "$WORKDIR/canon.out" || { echo "FAIL: canonical_sha256 check did not fire"; cat "$WORKDIR/canon.out"; exit 1; }
echo "=== canonical tamper detected ==="

echo
echo "PASS: attestation build + verify round-trip clean"
