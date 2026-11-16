# Persistent-Rope ↔ TripleBuffer Composition

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.3.3] / `ft-2okh0.3.3`
**Status:** Foundation slice shipped — decision rubric +
shared-bytes estimator + retention-policy contract +
structured-log row + 26 lib tests all live. The
`TerminalState` migration (sub-task 2) and the bench
write-side (sub-task 4) are integration follow-on.

The persistent-rope substrate already lives at
`persistent_rope_grid.rs` and the triple-buffer substrate at
`triple_buffer.rs` / `watchdoged_triple_buffer.rs`. This
module ships the **composition contracts** that govern when
(and whether) to actually migrate `TerminalState` to a rope-
backed grid — i.e., the decision rubric the bead names.

## Headline rule

> Ship rope-backed-triple-buffer if AND ONLY IF: memory
> overhead with rope ≤1.5×, render performance unchanged,
> mutation performance unchanged. If the rubric says "don't
> ship rope": triple-buffer still ships with flat grid
> (3× memory acceptable).

## Decision rubric (sub-task 1 + 3)

`decide_rope_adoption(memory, perf) -> RopeAdoptionDecision`:

- **Memory check**: `max_overhead_ratio ≤ 1.5×` (the
  bead's stated number).
- **Render check**: `render_p99_rope / render_p99_flat ≤
  1.05×` (5% slack — "unchanged" with measurement noise).
- **Mutation check**: `mutation_p99_rope / mutation_p99_flat
  ≤ 1.10×` (10% slack — bead allows "should be fine" for
  O(log n) on rope vs O(1) on flat).

Rejection reasons (closed enum):

- `MemoryOverheadTooHigh` — bead's primary reject.
- `RenderRegression` — render path slowed.
- `MutationRegression` — insert/delete slowed beyond
  acceptable.
- `InsufficientData` — bench wasn't run.

## Memory-overhead measurement (sub-task 1 + 4)

`MemoryOverheadSample` captures one bench-time data point:

- `flat_three_copy_bytes` — baseline (3× single copy).
- `rope_three_root_bytes` — rope's 3 shared roots.
- `overhead_ratio()` returns rope-bytes / single-copy
  baseline. 1.0 = perfect sharing; 3.0 = no sharing.

`MemoryOverheadAggregate` collects samples across scenarios
(`idle_60s`, `200_pane_fleet`, `mutation_burst`); the
decision rubric reads the worst case.

The bead's "~1.1× typical" claim is verified by the
`typical_overhead_in_bead_target_range` and
`bead_target_11x_passes_decision` tests.

## Shared-bytes estimator (sub-task 4)

`SharedBytesEstimator` projects rope ref-counts onto the
structured-log shape:

- `total_bytes(chunks)` — sum of all chunk sizes.
- `shared_bytes(chunks)` — sum of chunks with `ref_count >
  1` (counted *once*, since the rope shares them).
- `average_sharing_pct(chunks)` — `shared / total * 100`.

## Old-snapshot retention contract

`SnapshotRetentionPolicy` models the bead's "Hold a
snapshot from 60s ago; assert memory stable" requirement:

- Default threshold: `growth_ratio_threshold = 11_000`
  basis points (1.10× — 10% slack for measurement noise).
- `evaluate(rows) -> RetentionVerdict`:
  - `Stable` — memory growth within threshold.
  - `Unstable` — exceeded threshold (sharing failed; rope
    isn't dedup'ing across snapshots).
  - `InsufficientData` — fewer than 2 snapshots.

The `old_snapshot_60s_retention_scenario` test exercises
the bead's named scenario (3 snapshots over 60s with
+5% memory growth → `Stable`).

## Structured logging contract

`SnapshotLogRow` enum (tagged):

- `Snapshot { ts_ns, total_bytes, shared_bytes }` —
  per-snapshot row at
  `tests/rope_triple_buffer/logs/<scenario>.jsonl`.
- `SessionSummary { peak_memory_bytes,
  average_sharing_pct_x10000 }` — per-session summary.

`render_log_jsonl` / `parse_log_jsonl` are bidirectionally
clean.

## "DO NOT BREAK" rules

- **Snapshot-read consistency** — the foundation slice
  doesn't touch the triple-buffer protocol; the wrapper
  layer (`watchdoged_triple_buffer.rs`) keeps atomic snapshot
  reads regardless of whether the inner state is rope or
  flat.
- **A11Y snapshot semantics** — unchanged. The decision
  rubric chooses memory layout, not protocol; AT consumers
  see the same `TerminalState` API.

## Tests (26)

- 6 memory-overhead tests covering perfect / no / typical
  sharing + edge cases (empty aggregate, zero baseline).
- 6 decision-rubric tests covering Adopt + 4 rejection
  reasons + boundary case (1.5× exactly → Adopt).
- 4 shared-bytes-estimator tests (no/full/partial sharing
  + empty).
- 1 structured-log JSONL roundtrip.
- 3 retention-policy tests (stable, unstable, insufficient).
- 4 health-snapshot tests.
- 2 headline scenarios:
  `bead_target_11x_passes_decision`,
  `old_snapshot_60s_retention_scenario`.

## Bead acceptance status

| Item | Status |
|---|---|
| Decision rubric (memory ≤1.5× / render unchanged / mutation unchanged) | ✓ `decide_rope_adoption` |
| Memory-overhead measurement contract | ✓ `MemoryOverheadSample` + aggregate |
| Shared-bytes estimator | ✓ `SharedBytesEstimator` |
| Old-snapshot retention contract | ✓ `SnapshotRetentionPolicy` |
| Structured logging JSONL | ✓ `SnapshotLogRow` |
| Health snapshot for ft doctor | ✓ `RopeTripleBufferHealth` |
| Wait for `BR-TERM-EMULATOR-UPLIFT.2.5` rope ship | ✓ rope substrate exists at `persistent_rope_grid.rs` |
| Migrate TerminalState to rope-backed grid | ⏳ depends on rubric outcome (run bench first) |
| 200-pane fleet memory bench | ⏳ integration follow-on (write-side bench) |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Substrate: `persistent_rope_grid.rs`, `triple_buffer.rs`,
  `watchdoged_triple_buffer.rs`.
- Sibling: `ft-2okh0.3.1` (TripleBuffer foundation), `ft-l0oe3`
  (WatchdogedTripleBuffer GUI integration — same family),
  `ft-q6x91` (render-path migration to TripleBuffer.read()).
- Attestation: `ft-syqcz.1`.
