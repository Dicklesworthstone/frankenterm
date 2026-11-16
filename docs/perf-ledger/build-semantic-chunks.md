# build_semantic_chunks — perf ledger (ft-o2mtn)

## Scope

Full deterministic windowing pipeline an `ft.recorder.chunking.v1`
ingest call traverses, measured at 100 / 1000 / 10000 recorder
events across two policy configurations (default vs smaller chunks).
Sibling bench to ft-3r0n4 (wa.state fleet envelope) — that one
measures envelope serialization, this one measures the chunking
step that feeds the search-index pipeline.

Source: `crates/frankenterm-core/src/search/chunking.rs:242` (re-exported via `frankenterm_core::search::build_semantic_chunks`).

## Pipelines benched

| Pipeline | Steps included |
|----------|----------------|
| `semantic_chunks_construct_only` | Build `Vec<ChunkInputEvent>` only (no chunking) |
| `semantic_chunks_default` | Construct + `build_semantic_chunks` (default policy: 1800 chars / 48 events / 120s window) |
| `semantic_chunks_smaller` | Construct + `build_semantic_chunks` with `max_chunk_chars=512`, `max_chunk_events=16`, `overlap_chars=64` |

Subtraction of group medians attributes:
- `chunking_default = semantic_chunks_default - semantic_chunks_construct_only`
- `chunking_smaller = semantic_chunks_smaller - semantic_chunks_construct_only`
- `smaller_overhead = chunking_smaller - chunking_default`

## Adversarial fields

The synthetic event generator avoids degenerate "single monster
chunk" measurements:

- Pane IDs cycle 0..3 (forces direction/pane hard boundaries)
- Half ingress, half egress (forces direction-change boundaries)
- Every 17th event is a `ControlMarker` (forces hard-boundary flush)
- Text length varies 8..256 chars (forces soft-split + overlap branches)
- Session IDs cycle every 256 events (exercises mixed-session glue)
- Offsets stride one segment per 1024 events (exercises segment boundary)

## Hypothesis ledger

Pre-measurement predictions, to be falsified or supported by
criterion output. The profiling skill mandates writing these
BEFORE the bench runs so confirmation bias doesn't bend the
analysis.

| ID | Hypothesis | Verdict |
|----|------------|---------|
| H1 | `construct_only` scales linearly: 100 ≈ k, 1000 ≈ 10k, 10000 ≈ 100k. Synthetic builder is `Vec::with_capacity` + N `RecorderEvent` JSON-style construction; no algorithmic surprise. | pending |
| H2 | At 10000 events, `ordered.sort_by_key(...)` (chunking.rs:251) is a non-trivial fraction of `chunking_default` cost. Sort runs unconditionally even when input is already sorted. Predict ≥ 5% of total. | pending |
| H3 | `chunking_smaller` is 1.5–3× SLOWER than `chunking_default` at 10000 events. Reason: smaller `max_chunk_chars` produces more chunks → more `tail_chars` overlap copies (chunking.rs:481, O(N) String allocation per soft split) → more `sha256_hex` content-hash recomputations (chunking.rs:208). | pending |
| H4 | Output chunk count for default config at 10000 events: 200–500 chunks. Reason: average ~125 chars/event × 10000 events = ~1.25 MB; with 1800 char budget per chunk and frequent boundaries, ~600 chunks pre-glue, then glue rules merge tiny fragments. | pending |
| H5 | Peak heap is dominated by `ordered = events.to_vec()` clone (chunking.rs:250). At 10000 events this is ~80 KB of `ChunkInputEvent` (Vec<ChunkInputEvent> with String text fields), while output chunks total ~1.5 MB. Predict heap high-water > input clone but the clone is the largest single allocation event. | pending |

## Methodology

Benchmark collection is remote-required RCH work. Keep the inner
`cargo bench` command behind `rch exec` with `RCH_REQUIRE_REMOTE=1`;
do not use local benchmark wrapper output as perf-ledger evidence.

```
env -u CARGO_TARGET_DIR \
  RCH_REQUIRE_REMOTE=1 \
  RCH_VISIBILITY=verbose \
  RCH_NO_SELF_HEALING=1 \
  RCH_DAEMON_WAIT_RESPONSE_TIMEOUT_SECS=7200 \
  rch --no-self-healing exec -- env \
    CARGO_BUILD_JOBS=1 \
    CARGO_INCREMENTAL=0 \
    CARGO_TARGET_DIR=/tmp/ft-o2mtn-semantic-chunks-target \
    cargo bench -p frankenterm-core --bench semantic_chunks
```

Compare against the criterion baseline saved by previous retained RCH runs:

```
env -u CARGO_TARGET_DIR RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env \
  CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  CARGO_TARGET_DIR=/tmp/ft-o2mtn-semantic-chunks-baseline-target \
  cargo bench -p frankenterm-core --bench semantic_chunks -- --save-baseline ft-o2mtn
# … later:
env -u CARGO_TARGET_DIR RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env \
  CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  CARGO_TARGET_DIR=/tmp/ft-o2mtn-semantic-chunks-compare-target \
  cargo bench -p frankenterm-core --bench semantic_chunks -- --baseline ft-o2mtn
```

CI integration: `scripts/check_bench_budgets.sh` reads
`target/criterion/wa-budgets.json` and fails the build if any
group's median exceeds the `bench_common` threshold table.

## Measured (CI fills this in)

Numbers below are pending retained RCH measurement — do not populate
them from local Cargo output. Each value is criterion's reported
median; throughput shows elements/sec for all groups.

```
| Pipeline                  | 100 evts  | 1000 evts | 10000 evts | scale 10000/100 |
|---------------------------|-----------|-----------|------------|-----------------|
| construct_only            | _ µs      | _ µs      | _ µs       | _ ×             |
| chunk_default             | _ µs      | _ µs      | _ µs       | _ ×             |
| chunk_smaller             | _ µs      | _ µs      | _ µs       | _ ×             |
| chunking_default (derived)| _ µs      | _ µs      | _ µs       | —               |
| chunking_smaller (derived)| _ µs      | _ µs      | _ µs       | —               |
| smaller_overhead (derived)| _ µs      | _ µs      | _ µs       | —               |
```

The fingerprint header (per the profiling skill's contract) goes
alongside each populated row: CPU model + cores + governor +
kernel + toolchain + LTO mode + same-host validation.

## Hand-off

Per the profiling skill: this bead stops at the hotspot table.
If H2 (sort dominates at fleet scale) is supported, the
optimization (presort-detection short-circuit, or replace
unconditional sort with `is_sorted_by_key` guard) becomes a
follow-on bead routed to `/extreme-software-optimization`.

If H3 is supported (smaller chunks 1.5–3× slower), the
optimization candidate is `tail_chars` (chunking.rs:637) — a
linear `chars().count() + chars().skip()` scan that becomes
quadratic across many soft splits — and could be replaced with a
single `char_indices().rev()` scan.

If H5 is supported (clone dominates), the optimization is to make
`build_semantic_chunks` accept `Vec<ChunkInputEvent>` (consuming)
instead of `&[ChunkInputEvent]` so the existing caller (which has
already buffered the events) doesn't pay for the clone.

If H2 + H3 are both rejected (e.g., cost is dominated by
`sha256_hex` per chunk), the optimization is content-hash
batching — but routed to `sha2` upstream rather than ft itself.

## References

- `crates/frankenterm-core/benches/semantic_chunks.rs` — the bench
- `crates/frankenterm-core/src/search/chunking.rs:242` — `build_semantic_chunks`
- `crates/frankenterm-core/src/search/chunking.rs:251` — sort that may dominate (H2)
- `crates/frankenterm-core/src/search/chunking.rs:637` — `tail_chars` overlap copy (H3)
- `crates/frankenterm-core/src/search/chunking.rs:250` — `events.to_vec()` clone (H5)
- `crates/frankenterm-core/benches/wa_state_fleet.rs` (ft-3r0n4) — sibling bench, envelope
- `docs/perf-ledger/wa-state-fleet.md` — sibling perf ledger
