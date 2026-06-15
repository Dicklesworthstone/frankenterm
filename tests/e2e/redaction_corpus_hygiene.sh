#!/usr/bin/env bash
# W0.T (ft-7h5da.1.6) zero-RCH slice: redaction & corpus-hygiene verification.
#
# The headline binary e2e (plant canaries -> ft redact backfill -> grep
# output_segments/FTS5/Tantivy/embeddings -> backup -> purge) needs a running
# `ft` binary and W0.5 (catalog-stamping attestation), both currently
# unavailable (dep-upgrade compile meta-blocker + ft-7h5da.1.5 blocked). This
# script locks the parts verifiable WITHOUT a build, over committed core source
# + the test corpus + the attestation manifest:
#
#   * CORPUS HYGIENE: the committed test corpus/fixtures carry NO live secret of
#     any catalog family (regression guard so a future fixture cannot plant one).
#   * CATALOG COVERAGE: the redactor catalog has a pattern for every canary
#     family the binary e2e plants (JWT / GitLab / Twilio / SendGrid / Datadog),
#     so those canaries are catchable.
#   * RETROACTIVE-REDACTION FEATURE SHAPE (W0.2/W0.3, closed): backfill is
#     resumable + dry-runnable (idempotent re-run), its receipt stamps the
#     catalog version + per-family counts + a resume cursor + derived-store
#     (FTS/Tantivy/embeddings) rebuild flags, and the receipt carries NO
#     plaintext secret (hash/family-keyed only).
#   * BACKUP EXPORT stamps the redaction catalog version (W0.2).
#   * ATTESTATION SLOT: the security/redaction-hygiene slot is declared and its
#     artifact exists + parses.
#
# ZERO RCH / ZERO cargo. Core-only (does NOT touch main.rs — no collision with
# the concurrent robot-dispatch edits). Exit 0 IS the proof. Bead: ft-7h5da.1.6.
#
# Exit codes: 0 = all invariants hold; 1 = a failure; 2 = missing input.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || { echo "FATAL: cannot cd to repo root"; exit 2; }

REDACTOR="crates/frankenterm-core/src/redactor.rs"
BACKFILL="crates/frankenterm-core/src/redact_backfill.rs"
BACKUP="crates/frankenterm-core/src/backup.rs"
ATT_MANIFEST="docs/attestations/manifest.json"
ATT_ARTIFACT="docs/security/redaction-hygiene.json"

for f in "$REDACTOR" "$BACKFILL" "$BACKUP" "$ATT_MANIFEST"; do
  [[ -f "$f" ]] || { echo "FATAL: missing required input: $f"; exit 2; }
done
command -v jq >/dev/null 2>&1 || { echo "FATAL: jq not found"; exit 2; }

PASS=0
FAIL=0
pass() { printf 'PASS  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL  %s\n' "$1"; FAIL=$((FAIL + 1)); }

# Body slices for precise struct-field assertions.
CONFIG_BODY="$(awk '/pub struct RedactBackfillConfig/{f=1} f{print} f&&/^}/{exit}' "$BACKFILL")"
RECEIPT_BODY="$(awk '/pub struct RedactBackfillReceipt/{f=1} f{print} f&&/^}/{exit}' "$BACKFILL")"

echo "=== W0.T redaction & corpus-hygiene (zero-RCH slice, ft-7h5da.1.6) ==="
echo ""

# ---------------------------------------------------------------------------
# CORPUS HYGIENE — the committed corpus/fixtures must carry NO live secret.
# Distinctive, low-false-positive catalog families (the 5 e2e canary families
# + 4 high-distinctiveness extras). Datadog/AWS use keyed/prefixed forms to
# avoid matching benign hex blobs.
# ---------------------------------------------------------------------------
SECRET_FAMILIES='eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+|glpat-[A-Za-z0-9_-]{20,}|AC[a-fA-F0-9]{32}|SG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{40,}|(DD|DATADOG)_API_KEY[[:space:]]*[=:][[:space:]]*[a-f0-9]{32}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{36}|xox[baprs]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----'
CORPUS_HITS="$(git grep -nIE "$SECRET_FAMILIES" -- \
  'crates/frankenterm-core/tests/corpus/**' \
  'crates/frankenterm-core/tests/fixtures/**' \
  'crates/frankenterm-core/fixtures/**' 2>/dev/null || true)"
if [[ -z "$CORPUS_HITS" ]]; then
  pass "H1 committed corpus + fixtures carry no live secret of any catalog family"
else
  fail "H1 secret-shaped string(s) found in committed corpus/fixtures (corpus-hygiene leak):"
  printf '%s\n' "$CORPUS_HITS" | sed 's/^/        /' | head
fi

# ---------------------------------------------------------------------------
# CATALOG COVERAGE — every binary-e2e canary family is catchable.
# ---------------------------------------------------------------------------
declare -a FAMILIES=(JWT_TOKEN GITLAB_TOKEN TWILIO_ACCOUNT_SID SENDGRID_KEY)
cov_ok=1
for fam in "${FAMILIES[@]}"; do
  grep -q "static ${fam}:" "$REDACTOR" || { cov_ok=0; echo "        missing catalog pattern: ${fam}"; }
done
# Datadog is a keyed-name entry in SECRET_PATTERNS, not a standalone `static`.
grep -q 'DATADOG_API_KEY' "$REDACTOR" || { cov_ok=0; echo "        missing catalog coverage: DATADOG"; }
if [[ "$cov_ok" -eq 1 ]]; then
  pass "C1 redactor catalog covers all 5 e2e canary families (JWT/GitLab/Twilio/SendGrid/Datadog)"
else
  fail "C1 redactor catalog lost an e2e canary family — planted canaries would not be caught"
fi

# ---------------------------------------------------------------------------
# RETROACTIVE-REDACTION FEATURE SHAPE (W0.2/W0.3, closed).
# ---------------------------------------------------------------------------
if grep -q 'resume_after_id' <<<"$CONFIG_BODY" && grep -q 'dry_run' <<<"$CONFIG_BODY"; then
  pass "R1 backfill is resumable (resume_after_id) + previewable/idempotent-safe (dry_run)"
else
  fail "R1 backfill config lost resume_after_id/dry_run (resume + idempotent-rerun invariants)"
fi

r2_ok=1
for field in catalog_version segments_scanned segments_rewritten family_counts last_segment_id embeddings_invalidated fts_rebuilt tantivy_rebuild_requested; do
  grep -q "$field" <<<"$RECEIPT_BODY" || { r2_ok=0; echo "        receipt missing field: $field"; }
done
if [[ "$r2_ok" -eq 1 ]]; then
  pass "R2 backfill receipt stamps catalog_version + scanned/rewritten + per-family counts + resume cursor + derived-store rebuild flags"
else
  fail "R2 backfill receipt lost an observability/consistency field"
fi

# No-plaintext: the receipt + config must key on hash/family/version, never a raw secret.
if grep -qiE 'plaintext|secret_value|secret_text|raw_secret|secret_plaintext' <<<"$RECEIPT_BODY$CONFIG_BODY"; then
  fail "R3 backfill receipt/config carries a plaintext-secret field (would leak the canary into the receipt)"
else
  pass "R3 backfill receipt/config carries NO plaintext secret (catalog/family/id-keyed only)"
fi

# ---------------------------------------------------------------------------
# BACKUP EXPORT stamps the redaction catalog version (W0.2).
# ---------------------------------------------------------------------------
if grep -q 'redaction_catalog_version' "$BACKUP"; then
  pass "R4 backup export stamps redaction_catalog_version (retroactive backup redaction provenance)"
else
  fail "R4 backup manifest no longer stamps redaction_catalog_version"
fi

# ---------------------------------------------------------------------------
# ATTESTATION SLOT — declared + artifact exists + parses.
# ---------------------------------------------------------------------------
slot_path="$(jq -r '.slots[]? | select(.category=="security/redaction-hygiene") | .path' "$ATT_MANIFEST" 2>/dev/null)"
if [[ "$slot_path" == "$ATT_ARTIFACT" ]] && [[ -f "$ATT_ARTIFACT" ]] && jq empty "$ATT_ARTIFACT" 2>/dev/null; then
  pass "A1 security/redaction-hygiene attestation slot is declared, its artifact exists + parses"
else
  fail "A1 redaction-hygiene attestation slot/artifact missing or invalid (slot_path=$slot_path)"
fi

echo ""
echo "=== Summary: ${PASS} passed, ${FAIL} failed ==="
if [[ "$FAIL" -ne 0 ]]; then
  exit 1
fi
exit 0
