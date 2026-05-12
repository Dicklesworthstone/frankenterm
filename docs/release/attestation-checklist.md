# Attestation closure checklist

Per-release verification that the **attestation bundle** is
complete, signed, and re-verifiable offline. Run alongside
`docs/release/checklist.md` (the mandatory step list); this
file is the focused closer for `BR-RC-ATTESTATION-CLOSURE`
(ft-187kv).

## When to run

- **Before** tagging `vX.Y.0`. If the bundle is incomplete,
  fix the gap before tagging — the CI lane refuses to sign
  partial bundles.
- **After** the release CI workflow lands the signed bundle —
  re-verify offline as a third party would.

## Pre-flight: every category has a producing bead

The canonical required-category list lives in
[`docs/attestations/manifest.json`](../attestations/manifest.json)
(`required_categories` array). Before tagging, walk the list
and confirm each producing bead has shipped its artifact:

```sh
jq -r '.required_categories[]' docs/attestations/manifest.json
```

Also confirm every manifest slot declares proof taxonomy metadata:

```sh
jq -e '[.slots[] | select((.proof_categories // []) | length == 0)] | length == 0' \
  docs/attestations/manifest.json
```

For each category, check the corresponding "producing bead"
in [`docs/attestations/README.md`](../attestations/README.md)
"Required artifact categories" table:

| Category                            | Producing bead     | Verify         |
|-------------------------------------|--------------------|----------------|
| `perf/headline-claims`              | `ft-syqcz.3`       | `br show ft-syqcz.3` reports closed |
| `perf/competitor-matrix`            | `ft-e87u6.9`       | `br show ft-e87u6.9` reports closed |
| `perf/lindley-bounds`               | `ft-43x69`         | `br show ft-43x69` reports closed |
| `tui/render-parity`                 | `ft-35yac.1.2` + `ft-35yac.2` | both beads report closed |
| `security/passive-watch`            | `ft-x0666.1`       | `br show ft-x0666.1` reports closed |
| `security/redactor-coverage`        | `ft-x0666.2`       | `br show ft-x0666.2` reports closed |
| `security/distributed-threat-model` | `ft-x0666.3`       | `br show ft-x0666.3` reports closed |
| `proofs/loom-runtime-async`         | `ft-e87u6.12`      | `br show ft-e87u6.12` reports closed |
| `proofs/runtime-proof-trait`        | `ft-i2eni.1`       | `br show ft-i2eni.1` reports closed |
| `proofs/robot-contracts`            | `ft-0elb9`         | `br show ft-0elb9` reports closed |
| `doctrine/agents-md-counts`         | `ft-tf6g3.2`       | `br show ft-tf6g3.2` reports closed |
| `doctrine/cx-propagation`           | `ft-q0tz3`         | `br show ft-q0tz3` reports closed |

If any bead is **not closed**, the release MUST NOT proceed —
the attestation bundle would either be partial (rejected by
the build script) or claim coverage that doesn't exist.

## Build: assemble the bundle

```sh
scripts/attestation-build.sh --version 0.X.0 --channel stable --sign cosign
```

Offline fallback when keyless sigstore is unavailable:

```sh
ED25519_PRIVATE_KEY_PATH=release-ed25519.pem \
  scripts/attestation-build.sh --version 0.X.0 --channel stable --sign ed25519
```

Outputs:
- `docs/attestations/0.X.0.json` — the signed bundle
  (artifact paths + SHA-256 + size + producing-bead pointer).
- `docs/attestations/0.X.0.sigstore` — the cosign sigstore
  bundle (cert chain + signature over the canonical signing
  payload).
- `docs/attestations/0.X.0.ed25519.sig.hex` — the Ed25519
  fallback signature when using `--sign ed25519`.

The build script **fails loudly** on:
- Any required-category slot with `path: null`.
- Any artifact path that doesn't exist on disk.
- Any artifact whose hash differs from what the bead promised.
- Any manifest `proof_categories` ID that is not declared in
  `docs/proof-taxonomy.json`.

If the build fails, do NOT manually patch the manifest — fix
the producing bead's artifact instead. Manual edits silently
drift the schema.

## Verify: re-derive every hash

```sh
scripts/attestation-verify.sh docs/attestations/0.X.0.json
```

The verifier:
1. Parses the bundle.
2. Re-derives every artifact's SHA-256 from disk.
3. Recomputes the canonical signing payload.
4. Checks the `taxonomy_coverage` summary from
   `docs/proof-taxonomy.json`.
5. Verifies the sigstore signature against
   `COSIGN_IDENTITY` (the expected workflow ref), or verifies the
   Ed25519 signature against the bundle's `signature.public_key`.
6. Exits 0 on full pass; non-zero on any check failure.

For machine-readable output (CI gates):

```sh
scripts/attestation-verify.sh docs/attestations/0.X.0.json --json
```

The `--strict-required` flag adds: fail if the bundle's
`required_categories` list doesn't match the canonical
manifest. Use this in release CI.

## Tag + push: trigger the signed-build workflow

```sh
git tag v0.X.0 && git push origin v0.X.0
```

The release workflow at
[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
handles cosign keyless signing on tagged builds.

## Post-tag: third-party offline verification

Anyone (operator, security auditor, downstream packager) can
verify the published bundle without trusting GitHub:

```sh
git clone https://github.com/anthropics/frankenterm /tmp/ft-verify
cd /tmp/ft-verify
git checkout v0.X.0
scripts/attestation-verify.sh docs/attestations/0.X.0.json --strict-required
```

Exit code 0 means: every required artifact is present, every
hash matches, and the signature chains to the expected
identity. Anything non-zero is a release-integrity failure;
file a P0 bead and roll back if the bundle is in a public
release.

## Demonstration: missing artifact fails the build

The smoke test at
[`tests/attestation/smoke-test.sh`](../../tests/attestation/smoke-test.sh)
exercises both directions:

- **Positive**: build a complete bundle, verify it passes.
- **Negative**: tamper with one artifact's hash, verify the
  verifier rejects it.

Run as part of pre-tag validation:

```sh
bash tests/attestation/smoke-test.sh
```

A green smoke test means the attestation pipeline catches a
deliberate tampering. A red smoke test means the verifier is
broken — block the release until repaired.

## What this checklist replaces

Before this closure, each producing-bead epic could ship its
artifact, mark itself complete, and the release would tag
without anyone confirming the FULL bundle was ready. With
this checklist:

1. Every release runs the pre-flight category sweep.
2. The build script refuses partial bundles.
3. The verifier re-derives everything.
4. The smoke test asserts tamper detection.

Net: a bundle that passes this checklist is **complete**
(every required category present), **content-addressed**
(every artifact's hash baked in), and **signed**
(cosign keyless via the GitHub Actions workflow ref).

## Cross-references

- [`docs/attestations/README.md`](../attestations/README.md)
  — bundle-format reference, build/verify commands.
- [`docs/attestations/schema.json`](../attestations/schema.json)
  — JSON Schema 2020-12 for the bundle format.
- [`docs/attestations/manifest.json`](../attestations/manifest.json)
  — canonical required-categories list.
- [`docs/proof-taxonomy.json`](../proof-taxonomy.json)
  — proof category registry used by `proof_categories` and
  `taxonomy_coverage`.
- [`scripts/attestation-build.sh`](../../scripts/attestation-build.sh)
  — bundle assembler.
- [`scripts/attestation-verify.sh`](../../scripts/attestation-verify.sh)
  — bundle verifier.
- [`tests/attestation/smoke-test.sh`](../../tests/attestation/smoke-test.sh)
  — positive + tamper-detection smoke test.
- [`docs/release/checklist.md`](checklist.md) — broader
  release checklist; this attestation-checklist.md is the
  focused closer.
- [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
  — the doctrine that motivates the attestation graph.
- BR-RC-FOUNDATION.G3.1 (`ft-syqcz.1`, closed) — schema +
  build + verify pipeline.
- BR-RC-FOUNDATION.G3.1.1 (`ft-syqcz.1.1`, open) —
  user-facing `ft attestation verify` CLI.
- BR-RC-ATTESTATION-CLOSURE (`ft-187kv`, this bead) —
  per-epic closure.
