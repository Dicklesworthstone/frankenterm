# Snap-Back Repaint + Adversarial Resize Fuzz

**Bead:** [BR-TERM-EMULATOR-UPLIFT.2.4] / `ft-mpc9b.2.4`
**Sub-epic:** 2 — Live-Resize Fast Path

## Why this exists

Sub-epic 2's three layers (`ft-mpc9b.2.1` live-resize state
machine, `ft-mpc9b.2.2` draft-mode policy, `ft-mpc9b.2.3`
incremental reflow) compose into the user-perceived behaviour:
gesture starts → render in Draft → gesture ends → snap back to
Standard → settle to steady-state.

Each layer is exercised in isolation by its own fixture. This
bead exercises the **composition** with adversarial inputs:
random gesture sequences, random Configure dimensions, random
WatchdogTick + MouseUpDuringResize injection. The headline
correctness rules:

1. **Quiescent equality.** A gesture sequence ending at `Idle`
   is observationally identical to never having gestured — same
   render quality, same feature flags, no state drift.
2. **Snap-back idempotency.** Within a single gesture
   (Begin..End span), exactly ONE Standard snap-back fires.
   Back-to-back gestures each get their own one-snap-back
   budget.
3. **Independence-rule preservation.** A11Y / color / IME
   independence holds across every snap-back regardless of how
   pathological the gesture sequence is.

## What this module ships

`crates/frankenterm-core/src/snap_back_fuzz.rs`:

- **`FuzzSeed`** — local xorshift64* PRNG (same shape as
  `frankenterm-gui::gpu_regression_fuzz::FuzzSeed`, copied
  in-core to avoid a `core → gui` dep edge).
- **`GestureFuzzConfig`** — knobs (event budget, max
  width/height, max duration).
- **`GestureFuzzStream`** — bounded iterator of
  `LiveResizeEvent` driven by a seed. Distribution: 10 %
  BeginSignal / 50 % Configure / 10 % EndSignal / 5 %
  MouseUpDuringResize / 25 % WatchdogTick. Timestamps strictly
  monotonic.
- **`run_fuzz_seed`** — runs a stream through the live-resize
  state machine + draft-mode driver and emits a
  `SnapBackFuzzResult` listing per-event observations + any
  `SnapBackViolation`s.
- **`SnapBackViolation`** — 4 named violations:
  `SnapBackNotStandard`, `DoubleSnapBackInGesture`,
  `QuiescentDriftFromSteadyState`,
  `IndependenceRuleViolated`.
- **`SnapBackDivergenceReport`** + JSONL writer for the bead's
  `tests/snap_back_fuzz/logs/<seed>.jsonl` schema.

## Real bugs the harness caught

The fuzz harness found **two real bugs** in my own gesture-
tracking logic during development:

1. **Multi-gesture span counting.** My initial `in_gesture`
   reset condition was Idle-only, but the live-resize state
   machine can collapse `ResizeEnd → Idle → ResizeBegin` into
   a single observable transition. Multiple back-to-back
   gestures each fired a snap-back, but they all counted into
   the same gesture span — 2-4 snap-backs per "gesture",
   which the idempotency rule rejected.
   **Fix:** snap-back transition closes the current gesture
   immediately, regardless of whether `Idle` is observed
   between gestures.
2. **Post-stream cleanup.** The watchdog tick at
   `max_duration_ms + 10_000` correctly forces ResizeEnd, but
   the state machine's auto-clear from `ResizeEnd → Idle`
   needs a *follow-up* event. My initial cleanup left the
   final state at `ResizeEnd` instead of `Idle`, failing the
   quiescent-equality rule.
   **Fix:** feed two trailing watchdog ticks; the second
   flushes ResizeEnd → Idle.

Both are real implementation bugs the unit/scenario tests
hadn't surfaced. The adversarial fuzz path produced
deterministic seed-replayable repros (seed 0, seed 123) that
made the bugs trivial to debug.

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| ResizeEnd handler triggers snap-back repaint | ✓ (DraftModeDriver from 2.2) |
| Skip animation: instantaneous quality transition | ✓ (snap-back is exactly one frame) |
| Adversarial fuzz harness | ✓ (this module) |
| Random gesture + content sequences | ✓ (GestureFuzzStream) |
| Quiescent equality at Idle | ✓ (`run_fuzz_seed` checks this) |
| Snap-back idempotency property | ✓ (Rule 2 enforced by checker) |
| A11Y / color / IME preservation | ✓ (Rule 3 enforced by checker) |
| Deterministic seeds for repro | ✓ (FuzzSeed reproducibility test) |
| 24h CI lane | ⏳ CI infrastructure follow-on |
| Real-user dogfood week | ⏳ ship the integration first |
| VHS SSIM ≥0.999 final frame | ⏳ GPU integration bead |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`.
- **Upstream consumers:**
  - `live_resize` state machine (`ft-mpc9b.2.1`) — the
    machine the fuzz events drive.
  - `render_quality` driver (`ft-mpc9b.2.2`) — the
    quality-picking policy the fuzz harness validates.
- **Independence-rule cross-references:**
  - `a11y_tree_update` → `ft-mpc9b.10.1`.
  - `color_profile` → `ft-mpc9b.10.3`.
  - `ime_caret_anchor` → `ft-mpc9b.10.2`.
- **24h CI lane:** filed as follow-on under the parent epic.
- **Real-GPU SSIM ≥ 0.999 verification:** GPU integration
  bead — uses the `gpu_regression_fuzz` infra from
  `ft-mpc9b.1.6` parameterized by gesture sequences from this
  module's `GestureFuzzStream`.

## How a future GPU-integration bead consumes this

```rust
// Pseudo-code for the future GPU-integration bead.
for seed in 0..fuzz_corpus_size {
    let result = snap_back_fuzz::run_fuzz_seed(
        seed,
        GestureFuzzConfig::default(),
        SteadyStateQuality::Standard,
    );
    assert!(result.violations.is_empty());

    // Replay the same gesture stream against the GPU; capture
    // the final frame; SSIM-compare against a no-gesture
    // reference render.
    let final_frame = render_with_gestures(seed);
    let reference_frame = render_without_gestures();
    assert!(ssim(&final_frame, &reference_frame) >= 0.999);
}
```
