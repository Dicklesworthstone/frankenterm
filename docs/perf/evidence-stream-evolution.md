# Evidence-stream schema evolution

Canonical record of changes to the per-claim evidence-stream JSONL row shape. The current schema lives at `docs/perf/evidence-stream-schema.json` and the Rust source-of-truth is `crates/ft-perf-gate/src/lib.rs` `EvidenceSample`.

This doc is the home for migration notes that the proof-gate consumers (SPRT, conformal, Lindley latency bounds, observed-delay heavy-tail quantile bounds, regime-shift, causal-DAG, headline-claim attestation) must honor when reading older or newer rows.

## Versioning rules

1. **Schema version is a string constant** declared as `ft.perf.evidence-sample.vN`. The constant lives both in the JSON schema (`properties.schema_version.const`) and in the Rust source as `EVIDENCE_SAMPLE_SCHEMA_VERSION`. The two must agree; G56 (`ft-tf6g3.44`) meta-validator enforces this.

2. **Bumps are MAJOR only.** Adding a new optional field is *not* a bump (existing rows remain valid). Adding a new required field, removing a field, or changing a field's type *is* a bump. The MAJOR-only policy keeps consumers' migration matrix small.

3. **Bump procedure:**
   - Add the new `EvidenceSample` shape in `lib.rs` alongside the old one (or use a versioned enum).
   - Bump `EVIDENCE_SAMPLE_SCHEMA_VERSION` to `ft.perf.evidence-sample.v(N+1)`.
   - Update `docs/perf/evidence-stream-schema.json` to match the new shape.
   - Add a `## v(N-1) → vN` section to this doc declaring the migration.
   - Ship a migration helper in `crates/ft-perf-gate/src/migration.rs` that reads vN-1 rows and rewrites them to vN.
   - All consumers (G25, G26, G42, G43, G47, G54) read EITHER vN-1 or vN until the deprecation window passes (default 90 days).
   - Tag deprecation cutoff in the release attestation bundle (G16 `ft-tf6g3.1`).

4. **Deprecation cutoff** is the release tag at which vN-1 rows are no longer accepted by gates. After cutoff, vN-1 rows are still readable by the migration helper for forensic / replay purposes but produce a `'schema-vN-1-deprecated'` warning.

5. **Fixture preservation.** When a bump lands, the fixture corpus in `tests/fixtures/evidence-corpus/` (G54 `ft-tf6g3.42`) ships both vN-1 and vN snapshots so the migration helper has regression test coverage.

## Validator placement

- **At write time:** every bench harness that emits rows MUST use the canonical helper (`ft-test-log` from G52 `ft-tf6g3.40`); the helper guarantees schema_version is stamped correctly.
- **At read time:** every consumer crate (`ft-perf-gate`, `frankenterm-core/benches/...`) MUST validate the row's `schema_version` field against its supported set before deserializing.
- **In CI:** a workflow step runs `scripts/validate-perf-evidence.sh` against every JSONL emitted under `target/test-logs/**/*.jsonl`. Failure blocks merge.

## Sibling artifacts that move with the schema

A bump may also need synchronized updates to:

- `docs/perf/headline-claims.json` if claim_id naming convention changes
- `docs/perf/competitor-matrix.json` if a comparison row uses the schema
- `docs/perf/bench-coverage-matrix.json` if claim or workload-class enumeration changes
- `docs/attestations/schema.json` (`evidence_stream_version` slot) if the bundle records the active schema version
- `crates/ft-perf-gate/Cargo.toml` if a new dependency is needed (rare)

## Version log

### v1 (2026-05-12 — current)

Initial release. Substrate for `ft-tf6g3.32` (G47). Source: `crates/ft-perf-gate/src/lib.rs` shipped under commit `ceaee1622`.

**Required fields:** `schema_version`, `ts_ms`, `claim_id`, `metric_value`, `metric_unit`, `sample_size`.

**Optional fields:** `commit_sha`, `hardware_fingerprint`, `runner_sku`, `workload_class`, `tags`.

**Reserved tag keys:** `feature_flags`, `os_version`, `kernel_version`, `gpu_vendor`, `gpu_model`, `frankenterm_version`.

**Known constraints:**
- `metric_value` must be finite (no NaN, no infinity).
- `claim_id` must match `^[A-Za-z][A-Za-z0-9_.-]*$`.
- `commit_sha` (when present) must match `^[0-9a-f]{7,40}$`.

**Downstream consumers at v1:**
- `ft-perf-gate::sprt::evaluate_sprt` (G25 `ft-tf6g3.10`)
- `ft-perf-gate::conformal::fit_band_from_samples` (G26 `ft-tf6g3.11`)
- `ft-perf-gate::regime_shift::detect_from_samples` (G42 `ft-tf6g3.27`)
- `ft-perf-gate::causal_attribution::rank_attribution_candidates` (G43 `ft-tf6g3.28`)
- `docs/perf/headline-claims.json` (G19 `ft-tf6g3.4` children)
- `docs/perf/latency-derivation.md` (G23 `ft-tf6g3.8`, future G38 `ft-tf6g3.23`)

**Open follow-ons for future versions:**
- schemars-derived JSON-Schema export from the Rust struct (deferred to a follow-on bead; today's schema is hand-written and parity-asserted by visual inspection)
- Optional `experiment_id` field for A/B-flighted measurements (would be v2)
- Histogram-bucket support for non-scalar measurements (would be v2; requires breaking `metric_value: number` → `metric_value: number | histogram`)

## Cross-references

- Schema file: [`docs/perf/evidence-stream-schema.json`](evidence-stream-schema.json)
- Rust struct: `crates/ft-perf-gate/src/lib.rs`
- Schema-version constant: `EVIDENCE_SAMPLE_SCHEMA_VERSION`
- Bead: `ft-tf6g3.32` (G47)
- Substrate parent: `ft-tf6g3.30` (G45 `ft-perf-gate` substrate crate)
- Downstream: `ft-tf6g3.42` (G54 fixture corpus)
