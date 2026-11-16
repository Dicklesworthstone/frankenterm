# FrameBudget → Redraw-Predicate Signal Coupling

**Bead:** [BR-TERM-EMULATOR-UPLIFT.5.2.cont] / `ft-s0nah`
**Status:** Foundation slice shipped — pure-logic adapter
substrate landed in core. `paint.rs` wiring (sub-task 1),
heavy-burst bench (sub-task 6), and `FrameBudgetTelemetry`
projection from the gui-crate `FrameBudget` (sub-task 4
write-side) are the integration follow-on.

The `FrameBudget` allocator already lives in
`frankenterm-gui/src/termwindow/frame_budget.rs`
(substrate `bd1641aae`, 22 inline tests). This module
ships the pure-logic contracts the integration consumes,
without editing `paint.rs`.

## What this slice ships

### Sub-task 3 — Cosmetic-defer signal contract

`CosmeticDeferSignal` is the shape the redraw predicate's
`RedrawInputs` consumes. Bead requirement:

> When `TermWindow::should_paint()` lands (`ft-458t7`),
> gather `frame_budget.has_deferred_ops()` into the
> `cosmetic_defer_outstanding` `RedrawInputs` field.
> Non-empty queue → next frame must paint to drain it.

`must_paint_to_drain()` is the predicate the GUI
integration calls.

### Sub-task 5 — Reduce-motion policy

`decide_animation_defer(op_kind, would_exceed_budget,
reduce_motion) -> AnimationDeferDecision`. Bead's stated
rule:

> When the OS reports `reduce-motion=ON`, animations may
> be SKIPPED entirely (not deferred). When OFF,
> animations must be deferred (preserved) but never
> dropped from the queue.

Pure decision tree:

| `op_kind` | `would_exceed_budget` | `reduce_motion` | Decision |
|---|---|---|---|
| any | false | any | `Execute` |
| `Animations` | true | `On` | `Skip` |
| `Animations` | true | `Off` | `Defer` |
| not Animations | true | any | `Defer` |

### Sub-task 4 — Telemetry projection contract

`FrameBudgetTelemetrySnapshot` mirrors the bead's
"`queue_depth, lifetime_drops, lifetime_deferrals,
lifetime_bulk_drains, last_spent_ns, last_budget_ns`"
indicators, plus per-op-kind histograms for the
structured-log line (sub-task 1's emit shape).

`is_safe()`: queue depth ≤ 64 AND drop-rate ≤ 5% of
deferrals.

### Sub-task 7 — Sustained-burst harness

`SustainedBurstHarness` is a pure-state-machine harness
asserting the bead's "5 minutes of forced cosmetic
deferrals; assert queue depth stays bounded (never
exceeds `deferred_cap`); assert drop counter increments
as expected" requirement.

`run_burst(frames, pushes_per_frame, drains_per_frame)`
runs the burst; `queue_within_cap()` is the invariant.

The `five_minute_burst_at_60hz_stays_bounded` test runs
18,000 frames (5 min @ 60 Hz) of 2-push-1-drain pressure
and asserts:

- Queue depth ≤ deferred_cap (the bead invariant)
- At steady state queue is in [cap-1, cap]
- Drop counter incremented (saturation reached)

## Op-kind taxonomy

`OpKindSlug` mirrors the gui-crate `OpKind` 1:1:

| Variant | Slug |
|---|---|
| `DirtyQuadRebuild` | `dirty_quad_rebuild` |
| `Cursor` | `cursor` |
| `Selection` | `selection` |
| `Ligatures` | `ligatures` |
| `SubpixelAa` | `subpixel_aa` |
| `Decorations` | `decorations` |
| `Animations` | `animations` |
| `Plugin` | `plugin` |

`op_kind_slugs_match_gui_crate_field_names` test pins the
slug strings; the projection from the gui-crate `OpKind`
to this `OpKindSlug` is straightforward at the adapter
seam.

## "DO NOT BREAK" rules

- **A11Y reduce-motion** — the policy explicitly honors
  `reduce-motion=ON` by *skipping* (not deferring)
  animations under pressure. Tests:
  `over_budget_animations_with_reduce_motion_on_skips`,
  `animations_skipped_under_pressure_with_reduce_motion_on`
  (the headline scenario).
- **Cosmetic-defer must drain** — `CosmeticDeferSignal::
  must_paint_to_drain()` returns true iff the queue is
  non-empty. The redraw predicate cannot suppress paint
  when ops are pending.
- **Queue cap invariant** — `SustainedBurstHarness`
  invariant `queue_within_cap()` is enforced; saturation
  drops oldest (FIFO eviction).

## Tests (21)

- 2 op-kind-slug coverage tests (every-distinct, gui-
  crate name pinning).
- 2 cosmetic-defer signal tests (empty/non-empty).
- 4 reduce-motion policy tests (under-budget always
  Execute, non-Animations always Defer, Animations
  Skip-vs-Defer split).
- 5 telemetry-snapshot tests (baseline-safe, queue-
  depth boundary, 5%-drop-rate boundary, per-kind
  counter increments).
- 4 sustained-burst harness tests (within-cap,
  at-cap-drops-oldest, drain, push=drain).
- 1 5-minute @ 60Hz scenario (bead sub-task 7).
- 1 reduce-motion-on headline scenario.
- 1 telemetry serde roundtrip test.

## Bead acceptance status

| Sub-task | Status |
|---|---|
| 1 — paint.rs wiring (begin_frame / drain / try_execute / try_bulk_drain / end_frame) | ⏳ integration follow-on |
| 2 — Per-op cost estimation | ⏳ integration follow-on |
| 3 — `cosmetic_defer_outstanding` coupling | ✓ `CosmeticDeferSignal` shape ready |
| 4 — `FrameBudgetTelemetrySnapshot` doctor surface | ✓ contract; ⏳ gui-crate projection |
| 5 — A11Y.5 reduce-motion policy | ✓ `decide_animation_defer` |
| 6 — Heavy-burst bench (`benches/heavy_burst.rs`) | ⏳ integration follow-on |
| 7 — Sustained-burst regression test | ✓ `SustainedBurstHarness` + 5-min @ 60Hz scenario |
| 8 — Remove `#![allow(dead_code)]` from `frame_budget.rs` | ⏳ depends on sub-task 1 |

## Cross-references

- Substrate: `bd1641aae` (gui-crate FrameBudget, 22
  tests).
- Sibling: `ft-458t7` (`should_paint` predicate — same
  family of paint-policy contracts; the
  `cosmetic_defer_outstanding` field flows here),
  `ft-mpc9b.10.5` (reduce-motion preference plumbing —
  same A11Y substrate this slice's policy honors),
  `ft-mpc9b.5.1` (redraw-predicate substrate).
- Attestation: `ft-syqcz.1`.
