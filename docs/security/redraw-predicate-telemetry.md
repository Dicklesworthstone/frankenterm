# `should_paint()` Predicate Telemetry

**Bead:** [BR-TERM-EMULATOR-UPLIFT.5.1.cont] / `ft-458t7`
**Parent:** `ft-mpc9b.5.1` (predicate foundation shipped at
`52b8238bd` — `RedrawReason` / `RedrawInputs` /
`RedrawDecision` / `RedrawDecisionStats` + `evaluate()`).
**Status:** Foundation slice shipped. Telemetry contract +
OS-paint-source taxonomy + force-paint signal enum + 3-
scenario bench corpus + audit doc all live; production
`TermWindow::should_paint()` + `paint.rs:38` short-circuit +
per-platform OS-event handlers + bench source files require
GPU + Window stack and are integration follow-ons.

## Headline rule

> **≥99% idle skip rate** under a 10s idle session at 60Hz
> on a 12-pane fleet. RQ-S5/RQ-S8 in
> `docs/perf/resize-quality-slo.md`. Encoded as
> `RedrawDecisionHealth::meets_idle_skip_rq()`.

Plus three force-paint paths the predicate **MUST honor**:

1. **OS-paint request** — never drop `setNeedsDisplay`
   (macOS), `frame-callback` (Wayland — cross-link to
   `ft-mpc9b.3.2` / `ft-28opz`), or `ConfigureNotify` (X11).
2. **BEL / AT-update-pending** — accessibility tree pending
   updates and BEL alerts force paint.
3. **Cosmetic-defer outstanding** — cross-link to
   `ft-mpc9b.5.2` (frame-pacing budget allocator).

## Contract layer

`crates/frankenterm-core/src/redraw_predicate_telemetry.rs`:

- **`OsPaintSignalSource`** — 4-variant enum
  (`MacosSetNeedsDisplay`, `WaylandFrameCallback`,
  `X11ConfigureNotify`, `Synthetic`). The integration's
  per-platform OS-event handler latches a pending state per
  source.
- **`OsPaintLatch`** — Clear / Pending; `is_pending()`
  predicate.
- **`ForcePaintSignal`** — closed list of signals that force
  paint regardless of other inputs (`Bel`, `AtUpdatePending`,
  `CosmeticDeferOutstanding`).
- **`RedrawDecisionHealth`** — `*Health` snapshot with
  `evaluations_total`, `paints_total`, `skips_total`, plus
  per-reason / per-force-signal / per-OS-source counter maps.
- **`fold_decision`** — folds a `DecisionRecord` into the
  health.
- **`record_os_paint_consumption`** /
  **`record_force_paint`** — per-event counter recorders.
- **Predicates:** `meets_idle_skip_rq()` (≥99%) +
  `meets_typing_cadence_rq()` (≥40%).

## Bench corpus

3 named scenarios:

| Scenario | SLO | Acceptance |
|---|---|---|
| `Idle10s12PaneFleet` | RQ-S5 | `skip_rate_pct >= 99.0` |
| `TypingCadence1Hz` | RQ-S8 | `skip_rate_pct >= 40.0` (typing burns frames) |
| `ForcePaintEveryFrame` | RQ-S5 | `skips_total == 0` (no spurious skips when force-paint signals fire) |

`IdlePaintSkipBenchResult::evaluate` runs the per-scenario
acceptance; `IdlePaintSkipBenchSnapshot::all_pass()` is the
release gate.

## Tests

24 lib tests covering: distinct OS source slugs, latch
predicate, force-paint signal distinctness, baseline
vacuous-perfect, 99% + 40% + below-both bound transitions,
fold_decision counter increments, OS-paint consumption
counters, force-paint counters, all 3 bench scenario
acceptance cases (pass + fail), snapshot record dedup, serde
roundtrip, **`simulated_idle_10s_at_60hz_meets_idle_rq`** —
synthesizes the bead's headline 600-frame scenario at 99%
skip rate.

## Bead acceptance status

| Item | Status |
|---|---|
| Telemetry contract layer | ✓ `redraw_predicate_telemetry` module + 24 tests |
| OS-paint-source taxonomy (macOS / Wayland / X11) | ✓ `OsPaintSignalSource` |
| OS-paint latch (must-not-drop semantics) | ✓ `OsPaintLatch` |
| Force-paint signals (BEL / AT-pending / cosmetic-defer) | ✓ `ForcePaintSignal` |
| RQ-S5 99% idle-skip bound at type level | ✓ `meets_idle_skip_rq()` |
| RQ-S8 40% typing-cadence bound at type level | ✓ `meets_typing_cadence_rq()` |
| `TermWindow::should_paint()` method | ⏳ integration follow-on (GUI surgery) |
| `paint.rs:38` short-circuit wiring | ⏳ integration follow-on |
| Per-platform OS-event handlers (NSView setNeedsDisplay / Wayland frame-callback / X11 ConfigureNotify) | ⏳ integration follow-on |
| Bench source at `crates/frankenterm-core/benches/idle_paint_skip.rs` | ⏳ requires GPU runtime |
| ft doctor wiring (one-line projection) | ⏳ integration follow-on |
| Remove `dead_code` allow on `redraw_predicate.rs` | ⏳ removed when production callers exist |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Predicate foundation (parent):** `ft-mpc9b.5.1` shipped
  at `52b8238bd`;
  `crates/frankenterm-gui/src/termwindow/render/redraw_predicate.rs`
  carries `RedrawReason` / `RedrawInputs` / `RedrawDecision` /
  `evaluate()`.
- **Frame-pacing budget allocator:** `ft-mpc9b.5.2` —
  `cosmetic_defer_outstanding` signal feeds this predicate.
- **Wayland frame-callback substrate:** `ft-mpc9b.3.2` /
  `ft-28opz` (the latter shipped this session).
- **SLO:** `docs/perf/resize-quality-slo.md` — RQ-S5 (idle
  GPU), RQ-S8 (frame skip).
- **Sibling foundation fixtures** (same `*Health` /
  contract-layer pattern this session):
  `per_row_quad_cache_telemetry`,
  `elastic_buffer_gpu_telemetry`,
  `dec_2026_presentation_hold`, `iterm2_osc1337`,
  `osc_2x_cluster`, `gpu_regression_fuzz_report`,
  `wayland_compositor_matrix`, `tui_parity_oracle`, plus
  the 5 robot-family state machines and 5 safety proofs.
- **Attestation cross-link:** `ft-syqcz.1`.
