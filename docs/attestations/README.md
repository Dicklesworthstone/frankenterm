# FrankenTerm Release Attestation Bundles

Every reality-check claim in `README.md` and `AGENTS.md` is published through a content-addressed,
optionally-signed JSON bundle that lives in this directory. Anyone can re-derive the hashes,
verify the signature offline, and confirm exactly which guarantees are active in a given release.

This pipeline is defined by [`ft-syqcz.1`](#) (BR-RC-FOUNDATION.G3.1). The motivating doctrine is
in [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md).

## What's in a bundle

| File | Purpose |
|------|---------|
| `schema.json` | JSON Schema (2020-12) describing the bundle structure. Semver-stable; new categories require a schema bump. |
| `manifest.json` | Canonical declarative input list. Per-category slots map to the artifact path the producing bead must emit. The build script reads this. |
| `../proof-taxonomy.json` | Numeric proof taxonomy registry used by `proof_categories` metadata and bundle coverage summaries. |
| `<version>.json` | The signed bundle for a specific release. Lists every artifact (path + SHA-256 + size + producing-bead pointer) plus the signature info. |
| `<version>.sigstore` | (sigstore-signed bundles only) cosign sigstore bundle — Fulcio certificate, Rekor verification material, and signature over the canonical signing payload. |

## Required artifact categories

Every production-channel bundle must include at least one artifact for each of these categories.
The producing-bead column is the source of truth for which work item gates the artifact landing:

| Category | Producing bead | Bridge-plan section |
|----------|----------------|---------------------|
| `perf/headline-claims` | `ft-syqcz.3` | G3 |
| `perf/competitor-matrix` | `ft-e87u6.9` | G3 |
| `perf/lindley-bounds` | `ft-43x69` | G3 |
| `tui/render-parity` | `ft-35yac.1.2` (GPU visual adjunct) + `ft-35yac.2` (full ratatui<->ftui report) | G5 |
| `security/passive-watch` | `ft-x0666.1` | G9 |
| `security/redactor-coverage` | `ft-x0666.2` | G10 |
| `security/distributed-threat-model` | `ft-x0666.3` | G11 |
| `proofs/loom-runtime-async` | `ft-e87u6.12` | G8 |
| `proofs/runtime-proof-trait` | `ft-i2eni.1` | G1 |
| `proofs/robot-contracts` | `ft-0elb9` | G2 |
| `doctrine/agents-md-counts` | `ft-tf6g3.2` | G6 / G17 |
| `doctrine/cx-propagation` | `ft-q0tz3` | G14.2 |

The manifest can also include optional slots that are hashed into bundles when
present but are not in `required_categories` yet. Current optional slots include
`doctrine/vendored-provenance` (`ft-i2eni.6`) and `perf/atlas-packing`
(`ft-gtcm9.5`).

`proofs/robot-contracts` was introduced under `ft-q5njp` and refreshed under
`ft-0elb9`. It is repository evidence: it attests checked-in schema, golden
matrix, and test contracts for live profile apply, fleet scale, and fleet
rebalance receipt shapes plus unavailable, denied, and approval-required
envelopes. It does not claim those paths have been exercised in a production
deployment; release-specific production evidence belongs in the signed release
bundle.

## Building a bundle

```bash
# Production release (fails loudly if any required category is unfilled).
scripts/attestation-build.sh --version 0.2.0 --channel stable --sign cosign

# Offline Ed25519 signing fallback.
ED25519_PRIVATE_KEY_PATH=release-ed25519.pem \
  scripts/attestation-build.sh --version 0.2.0 --channel stable --sign ed25519

# Dev bundle — partial OK, signature optional.
scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned --allow-partial
```

`--sign cosign` requires `cosign` on PATH and `COSIGN_IDENTITY` in the environment (the
expected SAN/identity, typically the GitHub Actions workflow ref). The release CI lane
sets these automatically and records the emitted `.sigstore` file under
`signature.sigstore_bundle` with path, SHA-256, and size. The signing identity,
Fulcio/Rekor trust model, and third-party verification flow are documented in
[`SIGNING.md`](SIGNING.md). `--sign ed25519` requires `openssl`, `xxd`, and a
PEM-encoded Ed25519 private key path in `ED25519_PRIVATE_KEY_PATH`; it emits
`<version>.ed25519.sig.hex` beside the bundle and records the matching raw
32-byte public key in `signature.public_key`.

Each manifest slot can also declare `proof_categories`, using numeric IDs from
[`docs/proof-taxonomy.json`](../proof-taxonomy.json). The builder copies those
IDs onto delivered artifacts/deferred slots and emits `taxonomy_coverage` with
per-category counts, below-threshold flags, uncategorized artifact count, and an
optional delta from `FT_ATTESTATION_PRIOR_BUNDLE`.

## Verifying a bundle

```bash
# Re-derive every artifact's SHA-256 from disk + recompute the canonical signing payload
# + (when present) verify the sigstore or ed25519 signature.
scripts/attestation-verify.sh docs/attestations/0.2.0.json

# JSON output for AI/tooling consumers.
scripts/attestation-verify.sh docs/attestations/0.2.0.json --json

# CI mode — also fail if required_categories diverges from manifest.json
# or if any deferred slots remain in the bundle.
scripts/attestation-verify.sh docs/attestations/0.2.0.json --strict-required --strict-deferred
```

Exit code: `0` on full pass, `1` on any failure, `2` on usage error.

Ed25519 bundles use the same canonical signing payload. The schema records a
32-byte hex public key in `signature.public_key` and a repo-relative
`signature.signature_path` file containing the raw 64-byte signature as hex.
The verifier decodes both and checks the payload with OpenSSL.

Sigstore bundles also use the same canonical payload. The schema records
`signature.sigstore_bundle.path`, `signature.sigstore_bundle.sha256`, and
`signature.sigstore_bundle.size_bytes`; the verifier checks those bytes before
delegating to `cosign verify-blob` with the recorded certificate identity and
OIDC issuer.

## Canonical signing payload

To sign or verify, both sides must agree on byte-for-byte canonical input. The rule is:

> Take the bundle JSON, **delete the `.signature` field**, then run
> `jq --sort-keys --compact-output`. The resulting bytes are the canonical signing payload.
> Its SHA-256 is recorded in `signature.canonical_sha256` so verification can short-circuit
> any byte-level mismatch before invoking the (slow) crypto check.

This makes the signature stable under reordering of object keys but breaks under any change
to artifact paths, hashes, sizes, git commits, or release metadata — which is the whole point.

## Adding a new artifact category

1. Update `schema.json`'s `$defs.category` enum and bump `schema_version`.
2. Update `manifest.json` to add the slot (with `path: null` until the producing bead lands).
3. Add `proof_categories` IDs from `docs/proof-taxonomy.json` so coverage reports stay meaningful.
4. Update the table above.
5. Update README's "Trust & Attestation" section.

Schema versions follow semver. Adding an enum value is a minor bump (existing bundles still
validate); removing one is a major bump.

## Roadmap

- `ft attestation verify` CLI command — user-facing offline verification (tracked by [`ft-syqcz.1.1`](#)).
- Per-PR attestation diff — show which categories changed between releases.
