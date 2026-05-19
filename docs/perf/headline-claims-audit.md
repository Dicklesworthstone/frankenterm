# Headline-Claims Bench Audit

**Bead:** `ft-f5zyr` (BR-RC-FOUNDATION.G3.5).
**Manifest:** [`docs/perf/headline-claims.json`](headline-claims.json) (ft-syqcz.3).
**Schema:** [`docs/perf/bench-stats-schema.json`](bench-stats-schema.json) (ft-syqcz.2).
**Audit harness:** [`crates/frankenterm-core/tests/headline_claims_audit.rs`](../../crates/frankenterm-core/tests/headline_claims_audit.rs).

This doc covers the substrate-pass slice of ft-f5zyr: the
audit harness that regression-guards the manifest ↔ bench-source
contract without requiring actual `cargo bench` runs.

## What the audit verifies

The harness runs as a regular `cargo test` integration test and
asserts on a stable invariant of the manifest + bench-output
contract. 9 tests, all green:

| Test | Invariant |
| ---- | --------- |
| `manifest_parses_with_exactly_five_headline_claims` | The manifest lists exactly the 5 documented headline claims. |
| `manifest_carries_documented_claim_ids` | Every documented claim id (`capture_latency_p99`, `concurrent_panes_capacity`, `memory_per_pane_budget`, `zstd_compression_ratio`, `bloom_prefilter_speedup`) is present. |
| `every_claim_names_a_bench_file_that_exists` | Each `claim.bench.file` resolves to a real file under the repo. CI fails fast on rename/delete drift. |
| `every_claim_criterion_group_appears_in_the_named_bench_source` | Each `claim.bench.criterion_group` appears in the bench source — either as a literal string or as the head segment of a `criterion_group!(...)` macro invocation. |
| `every_claim_metric_kind_is_from_documented_enumeration` | `metric.kind` ∈ {`latency`, `throughput`, `memory`, `compression_ratio`, `speedup_ratio`}. |
| `every_claim_comparison_direction_is_semantically_consistent_with_kind` | `comparison_direction` is consistent with `metric.kind` — latency/memory ⇒ `lower_is_better`; throughput/compression_ratio/speedup_ratio ⇒ `higher_is_better`. |
| `bench_stats_schema_parses_and_has_documented_fields` | `bench-stats-schema.json` carries the documented `$id`, `$defs.distribution`, `$defs.percentile`, and the `contains q==0.999` rule. |
| `corpus_manifest_exists_for_zstd_compression_ratio` | `fixtures/perf/agent-output-corpus/MANIFEST` exists + declares the three documented fixture names. |
| `manifest_has_publishing_block_with_regression_gate` | The manifest's `publishing.regression_gate.method` is one of the documented statistical tests. |

The harness reads the manifest + bench source files directly. It
does **not** shell out to `cargo bench`, does **not** require
hardware-baseline runners, and runs in well under a second.

## What the audit intentionally does NOT verify

- **Actual bench runs.** Producing Distribution JSON artifacts
  is `cargo bench` infrastructure that requires
  hardware-baseline runners. Filed as **`ft-f5zyr.cont.runs`**.
- **Corpus content.** The MANIFEST schema + the three
  documented fixture names ship under this bead. Pinning
  concrete agent-output samples (with real sha256 values
  replacing the `TBD-cont-corpus` placeholders) is filed as
  **`ft-f5zyr.cont.corpus`**.
- **Per-PR regression gating.** The manifest's
  `publishing.regression_gate` block names the EBCI upper bound
  + 10% threshold; wiring that into a per-PR CI lane is
  `ft-9zzkg` (already referenced from the manifest).

## The 5 claims at substrate scope

| Claim id | Bench file | Bench group | Manifest cite |
| -------- | ---------- | ----------- | ------------- |
| `capture_latency_p99` | `crates/frankenterm-core/benches/delta_extraction.rs` | `delta_extraction` | <50ms capture latency on 4KB overlap |
| `concurrent_panes_capacity` | `crates/frankenterm-core/benches/mux_pool_scaling.rs` | `mux_pool/throughput_scaling` | 200+ concurrent panes |
| `memory_per_pane_budget` | `crates/frankenterm-core/benches/mmap_scrollback.rs` | `mmap_scrollback/store_append` | ~50MB / 100 panes; ~200MB / 200 panes |
| `zstd_compression_ratio` | `crates/frankenterm-core/benches/byte_compression.rs` | `byte_compression` | 5:1 to 10:1 zstd ratio |
| `bloom_prefilter_speedup` | `crates/frankenterm-core/benches/bloom_filter_ops.rs` | `bloom_filter/contains_miss` | 10–100× Bloom prefilter speedup |

Each row's bench file exists today and the harness verifies the
criterion group is wired correctly. **Producing Distribution
JSON artifacts** that satisfy the bead's "first-run" acceptance
is the wired-pass slice.

## Substrate vs wired-pass scope

Same pattern as ft-t9a6q.1 / .2 / .3 + ft-53zsr + ft-hac7w.2:

**Substrate-pass (this bead):**
- Audit harness regression-guarding the manifest ↔ bench-source contract (9 tests).
- Corpus MANIFEST scaffold with documented fixture names + placeholder sha256 column.
- This conventions doc.

**Wired-pass (named follow-ups):**
- **`ft-f5zyr.cont.runs`**: First-run distribution artifacts on hardware-baseline runners. Each bench produces `target/criterion/<bench>/wa-bench-meta.jsonl` lines validating against `bench-stats-schema.json`.
- **`ft-f5zyr.cont.corpus`**: Pinned agent-output samples. Replaces `TBD-cont-corpus` sha256 placeholders with real content hashes. Three fixtures: `repetitive` / `heterogeneous_agent_log` / `compressed_already`.
- **`ft-f5zyr.cont.runners`**: GitHub Actions workflow wiring `cargo bench` on the documented 3 hardware baselines (macos-14 M1, Apple M1 Pro local, ubuntu-24.04). Output goes to `docs/perf/releases/<version>.headline-claims.json` per the manifest's `publishing.release_artifact_path`.

## Cross-references

- [`docs/perf/headline-claims.json`](headline-claims.json) — the manifest the audit checks against.
- [`docs/perf/bench-stats-schema.json`](bench-stats-schema.json) — the per-bench Distribution record schema.
- [`docs/perf/bench-stats-spec.md`](bench-stats-spec.md) — methodology for sample size / EBCI / bootstrap CIs.
- [`crates/frankenterm-core/tests/headline_claims_audit.rs`](../../crates/frankenterm-core/tests/headline_claims_audit.rs) — the harness.
- [`fixtures/perf/agent-output-corpus/MANIFEST`](../../fixtures/perf/agent-output-corpus/MANIFEST) — corpus pinning substrate.
- ft-syqcz.2 / .3 — manifest + schema (closed predecessors).
- ft-syqcz.4 — Lindley derivation (consumes the first-run distributions; in_progress).
- ft-187kv — attestation closure for G3 (consumes the per-release artifact).
- ft-9zzkg — per-PR regression gate.
