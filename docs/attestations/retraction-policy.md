# Attestation Retraction Policy

Release attestation bundles are content-addressed claims, but a signed claim can
still become wrong after publication. When that happens, publish a signed
retraction instead of editing or silently superseding the original bundle.

## Record Shape

Retractions live under:

```text
docs/attestations/retractions/<original-bundle-sha256>/<slot-name>.json
```

`<slot-name>` is the affected bundle category with path separators replaced by
`__`, for example `perf__lindley-bounds.json`.

Each record contains:

```json
{
  "schema_version": "1.0.0",
  "retracted_at": "2026-05-13T00:00:00Z",
  "retracted_by_release": "0.3.1-corrigendum",
  "retraction_rationale": "The published Lindley bound omitted a heavy-tail workload class.",
  "affected_slot": "perf/lindley-bounds",
  "original_bundle_sha256": "<sha256 of the original bundle JSON>",
  "original_claim_value": {},
  "corrected_claim_value": null,
  "retraction_signature": {
    "method": "ed25519",
    "canonical_sha256": "<sha256 of the canonical retraction payload>",
    "signature_path": "docs/attestations/retractions/<hash>/perf__lindley-bounds.ed25519.sig.hex",
    "public_key": "<32-byte public key hex>"
  }
}
```

`corrected_claim_value` is `null` when the corrected claim is not known yet. If
the corrected value exists, publish it as JSON so downstream consumers can show
the replacement claim directly.

## Authority

Only maintainers authorized to sign release bundles may sign retractions. A
retraction without a valid `ed25519` or `sigstore-cosign-keyless` signature is
rejected by `scripts/attestation-verify.sh`; unsigned retractions must never be
treated as authoritative.

## Workflow

1. Identify the original bundle JSON and the affected slot category.
2. Write the rationale in a text file. The rationale must name the invalidated
   claim and the evidence that invalidated it.
3. Run:

   ```bash
   ED25519_PRIVATE_KEY_PATH=release-ed25519.pem \
     scripts/retract-bundle-slot.sh \
       --bundle docs/attestations/0.3.0.json \
       --slot perf/lindley-bounds \
       --rationale-file /path/to/rationale.txt \
       --retracted-by-release 0.3.1-corrigendum \
       --sign ed25519
   ```

4. Verify the original bundle:

   ```bash
   scripts/attestation-verify.sh docs/attestations/0.3.0.json --json
   ```

   The verifier must return `verdict: "retracted"` with exit code `3`.

5. Rebuild the next release bundle. `scripts/attestation-build.sh` includes all
   active records from `docs/attestations/retractions/` in its top-level
   `retractions` field so offline consumers learn about prior rescinded slots.

## Archival

Active retractions stay under `docs/attestations/retractions/<hash>/`. Records
older than the maintained forget window may move to
`docs/attestations/retractions/archive/<hash>/`, but verifiers still consult the
archive path on cache misses. The default forget window is five years unless a
release manager documents a different window in the release notes.

## Conflict Handling

If two signed retractions target the same `(original_bundle_sha256,
affected_slot)`, the latest record by transparency-log inclusion time should be
used when sigstore metadata is available. If only Ed25519 records are available,
the newest `retracted_at` wins and the conflict must be called out in the next
release notes.
