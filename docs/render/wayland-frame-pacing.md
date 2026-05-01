# Wayland Frame-Callback Pacing Fix

**Bead:** [BR-TERM-EMULATOR-UPLIFT.3.2] / `ft-mpc9b.3.2`
**Status:** Pacing helper + chain-depth ceiling guard shipped in
core; instrumentation already shipped in commit `151bde5fe`. Per-
compositor integration test (mutter / kwin / sway / hyprland) is
the explicit follow-on integration bead.

## The bug

Originally documented at
`frankenterm/window/src/os/wayland/window.rs:1146-1179`:

> Under fast resize, frame callbacks chain on every paint, the
> compositor floods, and paint stutters. Repro: drag a window
> edge rapidly back and forth on Wayland (any major compositor)
> for 5 seconds → text smears, lags behind cursor, phantom frames.
> Symptom: `frame_callback_chain_depth` grows beyond expected
> bound.

## What's already in place (from commit `151bde5fe`)

The instrumentation slice landed earlier:

- `frame_callback: Option<WlCallback>` field tracks pending
  callbacks.
- `frame_callback_chain_depth: u32` + `_peak: u32` counters.
- `do_paint()` early-return at line 1175 if a callback is
  already in flight (sets `invalidated=true`, returns).
- `next_frame_is_ready()` decrements the counter; if
  `invalidated` is set, kicks a deferred paint.
- `debug_assert` at line 1200 catches any path that reaches
  the `surface().frame()` request with a callback already
  pending.
- The `configure` / `window_configure` separation at line 578
  was audited and explicitly classified as **independent** of
  the frame-callback batching.

## What this commit ships

The bead's #3 ("chain-depth ≥3 guard, skip new requests until
current callback fires") was the only piece the instrumentation
hadn't provided. This module fills that gap with a **pure pacing
decision** the integration site consumes:

`crates/frankenterm-core/src/wayland_frame_pacing.rs`:

- `FramePacingState` — minimal observable state:
  `(pending_callback, chain_depth, invalidated)`. Maps onto
  `WaylandWindowInner`'s existing fields.
- `FramePacingDecision` — `Paint` / `Skip` / `SkipChainCeiling`.
- `decide(state) -> FramePacingDecision` — pure function the
  integration calls. The chain-depth ceiling fires
  unconditionally at `MAX_CHAIN_DEPTH = 3`, even if
  `pending_callback` is somehow false (defensive against state-
  drift bugs).
- `FramePacer` — counter machine wrapping `decide` + state +
  cumulative counters (`paints_total`,
  `skipped_pending_callback_total`,
  `skipped_chain_ceiling_total`, `coalesced_total`,
  `callback_fires_total`, `chain_depth_peak`). Replaces the ad-
  hoc bookkeeping in `window.rs`.
- `FramePacingEvent` — per-paint-attempt JSONL row matching the
  bead's structured-log schema:
  `{ts, frame_callback_pending, action_taken, chain_depth, compositor}`.
- `FramePacingHealth.has_hit_chain_ceiling()` — ft-doctor alert
  predicate; non-zero `skipped_chain_ceiling_total` is the
  signal.

## Invariants the regression net pins

The 15 lib tests cover:

1. **Idle state paints.** `decide(idle_state) == Paint`.
2. **Pending callback skips.** `decide(pending_state) == Skip`.
3. **Chain-depth ceiling fires.** Three tests pin
   `chain_depth >= MAX_CHAIN_DEPTH → SkipChainCeiling`,
   including the defensive case where `pending_callback` is
   `false` but the depth says otherwise.
4. **Coalescing.** Multiple Skip-decisions collapse: the first
   sets `invalidated`, subsequent skips bump `coalesced_total`.
5. **Callback fired returns deferred when invalidated.** The
   integration uses this return to fire a deferred paint after
   a callback drains the pending state.
6. **Realistic pattern stays at chain-depth ≤ 1.** 100
   paint/callback iterations leave `chain_depth_peak == 1`.
7. **Pathological pattern hits the ceiling.** Force-mutating
   chain depth to `MAX_CHAIN_DEPTH` causes the next request to
   return `SkipChainCeiling` and increments the counter.
8. **Resize-storm burst recovers.** 50 paint requests followed
   by 50 callback-fires drain the chain back to 0.
9. **JSONL round-trip identity.**

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| Replace per-paint frame_callback re-request with conditional logic | ✓ (instrumentation 151bde5fe) |
| Coalesce multiple invalidations into a single per-vsync paint | ✓ (existing early-return + this module's coalescing counter) |
| Chain-depth ≥3 guard | ✓ (this module, `MAX_CHAIN_DEPTH = 3`) |
| Audit similar bug at line 578 (configure paths) | ✓ (commit 151bde5fe — classified independent) |
| Per-compositor integration test | ⏳ follow-on (real Wayland environment) |
| Reproducer script demonstrating pre/post fix | ⏳ follow-on |
| Structured logging schema | ✓ (`FramePacingEvent` JSONL) |

## Wiring into `frankenterm/window/src/os/wayland/window.rs`

The integration lands in a follow-on bead; the touch points are
narrow:

```rust
// In WaylandWindowInner:
use frankenterm_core::wayland_frame_pacing::{FramePacer, FramePacingDecision};

pacer: FramePacer,  // replaces frame_callback_chain_depth + ad-hoc tracking

// In do_paint():
match self.pacer.request_paint() {
    FramePacingDecision::Paint => {
        // existing path: surface.frame() + dispatch NeedRepaint
    }
    FramePacingDecision::Skip
    | FramePacingDecision::SkipChainCeiling => {
        // The pacer already updated invalidated/counters.
        return Ok(());
    }
}

// In next_frame_is_ready():
if self.pacer.mark_callback_fired() {
    self.do_paint().ok();  // deferred paint
}
```

The window.rs change is small (~20 lines net); the load-bearing
correctness lives in the pure-function helper. Future renderer
beads (X11 ConfigureNotify burst, macOS NSView re-display) can
adopt the same `FramePacer` shape.

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `persistent_rope_grid`.
- **Live-resize state machine** (`ft-mpc9b.2.1`) — the upstream
  signal that drives the resize storm this pacer mitigates.
- **`ft doctor` surface:** `FramePacingHealth` mirrors the
  `*Health` shape from prior beads so future doctor wiring
  renders all of them side-by-side.

## What this is NOT

- A full per-compositor regression test. mutter / kwin / sway /
  hyprland / wayfire / weston run in their own CI lanes; the
  follow-on integration bead wires them.
- A fix for the line-578 configure separation. That was audited
  in `151bde5fe` and explicitly classified as independent.
- A reproducer script. The bead's repro requires a real
  Wayland desktop environment; the unit tests demonstrate the
  fix is correct, but the user-perceived "drag the edge for
  5s" demonstration is per-compositor.
