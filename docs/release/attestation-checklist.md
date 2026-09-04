# Attestation closure checklist

Per-release verification that the **attestation bundle** is
complete, signed, and re-verifiable offline. Run alongside
`docs/release/checklist.md` (the mandatory step list); this
file is the focused closer for `BR-RC-ATTESTATION-CLOSURE`
(ft-187kv).

## When to run

- **Before** tagging `vX.Y.0`. If the bundle is incomplete,
  fix the gap before tagging. DSR is the exclusive release orchestrator;
  a successful development-bundle check is not release qualification.
- **After** the DSR release path lands the signed bundle —
  re-verify offline as a third party would.

## Producing-bead closing convention

Every bead that produces or updates an attestation artifact MUST close with the
same manifest wiring discipline the release gate later enforces. Before closing
that bead, verify:

- [ ] Artifact file is at the path declared in
  [`docs/attestations/manifest.json`](../attestations/manifest.json), or the
  manifest path is updated in the same commit.
- [ ] `bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned`
  exits 0 without `--allow-partial` for non-deferred slots.
- [ ] `bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned --strict-deferred`
  exits 0 only if every previously deferred slot is now resolved.
- [ ] `bash scripts/attestation-verify.sh <bundle>` round-trip exits 0.
- [ ] `cargo test -p frankenterm-core --test readme_hedge_alignment` exits 0
  when the artifact changes README/AGENTS claim wording.
- [ ] `cargo test -p frankenterm-core --test attestation_manifest_completeness --no-default-features`
  exits 0.
- [ ] Capacity or memory-envelope wording changes also run
  `cargo test -p frankenterm-core --test swarm_capacity_resource_budget_model --no-default-features`
  and confirm `docs/attestations/perf/swarm-capacity-envelope.json` still keeps
  high-scale claims fail-closed unless the target-class artifact is non-skipped.
- [ ] Rehearsal-score wording changes cite the retained receipt or golden matrix
  path, preserve `blocked`, `missing_evidence`, `degraded`, `skipped`, and
  `fixture_only` states, and do not turn `score_percent` into a release claim.
  The optional manifest slot is `proofs/rehearsal-score`, currently backed by
  `crates/frankenterm-core/tests/fixtures/rehearsal_score_receipt_golden_matrix.json`.
- [ ] Deferred-proof replay wording changes cite the retained queue/harness
  artifact or live-attempt record, preserve `queued`, `wait_rch`,
  `dirty_overlap`, `prerequisite_blocked`, `stale_command`, `ambiguous`, and
  `completed` states, and do not turn a queued receipt into green proof. The
  optional robot-contract manifest slot is
  `docs/attestations/proofs/deferred-proof-replay.json`.
- [ ] Operating-envelope wording changes cite
  `docs/attestations/proofs/operating-envelope.json`, preserve the read-only
  admission scope, and keep target-class production capacity blocked
  until `docs/attestations/proofs/resource-cockpit-target-class.json` is
  non-skipped, passed, fresh, and authenticated for the exact source/target.
  The consumer's trust enforcement remains `ft-7h5da.10.4.4`.
- [ ] The closing comment cites the manifest slot category, artifact path,
  build/verify exit codes, and retained RCH artifact bundle path.

Use
[`docs/release/attestation-bead-closing-template.md`](attestation-bead-closing-template.md)
for the closing-comment shape. This convention exists because a bead that ships
an artifact but forgets the manifest slot can recreate the `ft-e87u6` NO_BEAD
gap. If you skip this checklist, the `ft-e87u6.5`
`attestation_manifest_completeness` regression test will fail CI.

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
| `perf/lindley-bounds`               | `ft-7h5da.10.2`    | `br show ft-7h5da.10.2` reports closed |
| `tui/render-parity`                 | `ft-35yac.1.2` + `ft-35yac.2` | both beads report closed |
| `security/passive-watch`            | `ft-x0666.1`       | `br show ft-x0666.1` reports closed |
| `security/redactor-coverage`        | `ft-x0666.2`       | `br show ft-x0666.2` reports closed |
| `security/distributed-threat-model` | `ft-x0666.3`       | `br show ft-x0666.3` reports closed |
| `proofs/loom-runtime-async`         | `ft-e87u6.12`      | `br show ft-e87u6.12` reports closed |
| `proofs/runtime-proof-trait`        | `ft-i2eni.1`       | `br show ft-i2eni.1` reports closed |
| `proofs/robot-contracts`            | `ft-0elb9`         | `br show ft-0elb9` reports closed |
| `doctrine/agents-md-counts`         | `ft-tf6g3.2`       | `br show ft-tf6g3.2` reports closed |
| `doctrine/cx-propagation`           | `ft-q0tz3`         | `br show ft-q0tz3` reports closed |

Optional release-support slots are still hashed when present, but they do not
appear in `required_categories` until the owning epic graduates them. For
rehearsal-score closeout, confirm the `proofs/rehearsal-score` slot remains
present and points at the golden matrix unless a later bead replaces it with a
stronger retained no-mock receipt bundle.
For deferred-proof replay closeout, confirm the optional
`proofs/robot-contracts` slot points at
`docs/attestations/proofs/deferred-proof-replay.json` and that the artifact
keeps queued/deferred receipts visibly distinct from completed remote proof.
For operating-envelope closeout, confirm the `proofs/robot-contracts` slot
points at `docs/attestations/proofs/operating-envelope.json`, that the artifact
records the retained fixture/proof-calendar counts, and that target-class
capacity remains blocked when the resource-cockpit target-class artifact is
`skipped_not_proven`.

If any bead is **not closed**, the release MUST NOT proceed —
the attestation bundle would either be partial (rejected by
the build script) or claim coverage that doesn't exist.

## Build: assemble the bundle

```sh
ED25519_PRIVATE_KEY_PATH=release-ed25519.pem \
  scripts/attestation-build.sh --version 0.X.0 --channel stable --sign ed25519
```

Run this as a retained DSR quality/release step. A DSR installation with an
explicitly configured non-GitHub OIDC issuer may instead select `--sign
cosign`; GitHub Actions identities and workflow refs are forbidden.

Outputs:
- `docs/attestations/0.X.0.json` — the signed bundle
  (artifact paths + SHA-256 + size + producing-bead pointer).
- `docs/attestations/0.X.0.ed25519.sig.hex` — the Ed25519
  signature when using the default DSR attestation path.
- `docs/attestations/0.X.0.sigstore` — an optional cosign sigstore
  bundle when DSR is configured with an explicit non-GitHub OIDC issuer.

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
5. Verifies the optional sigstore signature against the DSR-configured
   `COSIGN_IDENTITY`, or verifies the
   Ed25519 signature against the bundle's `signature.public_key`.
6. Exits 0 on full pass; non-zero on any check failure.

For machine-readable output (DSR quality gates):

```sh
scripts/attestation-verify.sh docs/attestations/0.X.0.json --json
```

The current verifier accepts unsigned development bundles and uses a
bundle-supplied Ed25519 key. That proves self-consistency, not release-owner
identity. Externally pinned trust and strict release verification are
unfinished in `ft-xxfwy.49`; do not close release integrity using exit zero
from this development verifier alone.

The `--strict-required` flag adds: fail if the bundle's
`required_categories` list doesn't match the canonical
manifest. Use this in the DSR release gate.

## Tag + publish: DSR only

```sh
dsr version tag frankenterm
dsr build frankenterm --version 0.X.0
dsr release frankenterm 0.X.0 --verify-tag
dsr release verify frankenterm 0.X.0
```

Use `--push` on the DSR tag command only when publication is explicitly
authorized. DSR owns the build, signing, checksum, provenance, publication,
and verification receipts. Do not invoke, inspect, or depend on a GitHub
Actions workflow at any point in this process.

## Post-tag: third-party offline verification

An operator can recheck artifact hashes in the existing primary checkout
at the intended release revision. Preserve dirty files and active sessions;
do not create a parallel checkout for this procedure:

```sh
git rev-parse HEAD    # must match the candidate identity in the receipt
scripts/attestation-verify.sh docs/attestations/0.X.0.json --strict-required
```

Exit zero establishes the implemented structural/hash checks. It does not
establish external signing authority, current-source runtime proof, or
passed target qualification. Finish `ft-xxfwy.49` and retain the configured
DSR release verification before making those claims. A failed verification
blocks the candidate; investigate the exact artifact and trust failure
before deciding on any published-release remediation.

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
(through the retained DSR signing path).

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
