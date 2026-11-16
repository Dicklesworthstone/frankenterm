# Per-Row Quad Cache Telemetry

**Bead:** [BR-TERM-EMULATOR-UPLIFT.1.5.cont] / `ft-556zx`
**Parent:** `ft-mpc9b.1.5` (foundation analyzer shipped at
`3e7f0d728` — `RowDecision`, `RowInvalidation`,
`RowInvalidationPlan`, `plan_from_dirty_bitmap`).
**Status:** Foundation slice shipped. Telemetry contract +
3-scenario bench corpus + audit doc all live; production
`LfuCache → Vec<Quad>` swap + paint-loop wiring + bench source
files require GPU runtime and are integration follow-ons.

## Headline rule

> **≥95% rows cache-hit per frame** for a 200-pane fleet
> typing 1 char/sec for 60s. RQ-S8 in
> `docs/perf/resize-quality-slo.md`. Encoded as
> `PerRowQuadCacheHealth::meets_rq_s8()`.

The bead replaces an LFU-evicted cache (wrong policy for
row-keyed lookup — could evict a row that hadn't actually
changed) with a **row-indexed** `Vec<Quad>` where slot `i`
maps to row `i` and the only legal eviction is resize-shrink
or pane teardown.

## Contract layer

`crates/frankenterm-core/src/per_row_quad_cache_telemetry.rs`:

- **`RowCacheEvent`** — 5-variant taxonomy:
  - `Hit { row }` / `Miss { row }` — paint loop emits per row reference.
  - `ResizeShrink { rows_evicted }` — only legal eviction trigger #1.
  - `PaneClosed { rows_evicted }` — only legal eviction trigger #2.
  - `FrameBoundary { rows_referenced }` — paint-loop boundary; rolls per-frame hit-rate, resets per-frame counters.
- **`PerRowQuadCacheHealth`** — `*Health` snapshot with
  lifetime `cache_hits_total` / `cache_misses_total` /
  `cache_evictions_total` / `frames_total` /
  `rows_referenced_total` + rolling `last_frame_hit_rate` +
  per-frame `frame_hits` / `frame_misses` accumulators.
- **`fold_event`** — bit-for-bit-faithful reducer.
- **`meets_rq_s8()`** — predicate the bench harness uses.
- **`evictions_are_resize_only()`** — structural predicate
  (always true under the closed event taxonomy; surfaced so
  ft doctor has a name to read).

## Bench corpus

3 named scenarios:

| Scenario | SLO | Acceptance |
|---|---|---|
| `FleetTyping200` | RQ-S8 | `lifetime_hit_rate_pct >= 95.0` after 200×60s typing |
| `ResizeShrinkRoundtrip` | RQ-S1 | every eviction is `ResizeShrink` or `PaneClosed` (no LFU drift) |
| `SynchronizedOutputRedraw` | RQ-S6 | `lifetime_hit_rate_pct >= 90.0` under heavy bracketed redraw |

`QuadCacheBenchResult::evaluate` runs the per-scenario
acceptance against final health; `QuadCacheBenchSnapshot::all_pass()`
is the per-release release gate.

## Tests

21 lib tests including:

- `simulated_fleet_typing_meets_rq_s8` — synthesizes the
  bead's headline scenario (200 panes × 60 frames × 24
  rows/pane × 1 miss + 23 hits per pane) and asserts
  RQ-S8 holds.
- `fleet_typing_passes_at_exact_bound` / `fails_below_bound`
  — boundary cases for the 95% bound.
- `synchronized_output_redraw_uses_90_pct_bound` — pin the
  cross-link to `ft-u6jos` (DEC 2026 presentation-hold)
  with its slightly-relaxed bound.
- `fold_frame_boundary_rolls_hit_rate_and_resets_frame_counters` —
  per-frame accumulators reset cleanly.

## Bead acceptance status

| Item | Status |
|---|---|
| Telemetry contract layer | ✓ `per_row_quad_cache_telemetry` module + 21 tests |
| Hit-rate / miss-rate / eviction counters | ✓ `PerRowQuadCacheHealth` |
| RQ-S8 acceptance bound at type level | ✓ `meets_rq_s8()` predicate |
| Replace LfuCache with row-indexed `Vec<Quad>` | ⏳ integration follow-on (frankenterm-gui surgery) |
| Paint-loop wiring (consume `RowInvalidationPlan`) | ⏳ integration follow-on |
| Bench source at `crates/frankenterm-core/benches/quad_cache_typing.rs` | ⏳ requires GPU runtime |
| ft doctor wiring (one-line projection) | ⏳ integration follow-on |
| Remove `dead_code` allow on `per_row_quad_cache.rs` | ⏳ removed when production callers exist |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Foundation analyzer (parent):** `ft-mpc9b.1.5` shipped at
  `3e7f0d728`; `crates/frankenterm-gui/src/termwindow/render/per_row_quad_cache.rs`
  carries `RowDecision` + `RowInvalidationPlan` +
  `plan_from_dirty_bitmap`.
- **Substrate:** `ft-tfzhy` (DirtyLineBitmap — input to the
  analyzer), `ft-hznqt` (ElasticBuffer — what the rebuilt
  quads concat into; sibling shipped this session).
- **Cross-link:** `ft-u6jos` (DEC 2026 presentation-hold) —
  the SynchronizedOutputRedraw bench scenario references it.
- **SLO:** `docs/perf/resize-quality-slo.md` RQ-S8 (typing
  hit-rate), RQ-S1 (resize-shrink eviction), RQ-S6 (heavy
  redraw).
- **Sibling foundation fixtures** (same `*Health` /
  contract-layer pattern this session):
  `elastic_buffer_gpu_telemetry`, `dec_2026_presentation_hold`,
  `iterm2_osc1337`, `osc_2x_cluster`,
  `gpu_regression_fuzz_report`, `wayland_compositor_matrix`,
  `tui_parity_oracle`, plus the 5 robot-family state machines
  and 5 safety proofs.
- **Attestation cross-link:** `ft-syqcz.1`.
