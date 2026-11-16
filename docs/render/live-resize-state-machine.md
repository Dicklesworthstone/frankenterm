# Live-Resize Gesture State Machine

**Bead:** [BR-TERM-EMULATOR-UPLIFT.2.1] / `ft-mpc9b.2.1`
**Sub-epic:** 2 — Live-Resize Fast Path

## Why this exists

Sub-epic 2's draft-mode rendering and incremental terminal grid
reflow both depend on a clean `LiveResizeState` signal: did the
user start a drag, are they still dragging, did they release? The
platforms disagree on how to report this and each has its own
failure modes:

| Platform | Native source | Failure mode |
| --- | --- | --- |
| **macOS** | `NSWindowWillStartLiveResize` / `NSWindowDidEndLiveResize` | Sometimes skips DidEnd on a fast release. |
| **Wayland** | `xdg_toplevel::configure` with `Resizing` state | Compositor cadences differ wildly (mutter vs sway vs Hyprland); a configure storm (>100 in 100ms) dirty-floods the renderer. |
| **X11** | `_NET_WM_STATE_LIVE_RESIZE` (cooperating WMs) + `ConfigureNotify` burst | False positives from workspace switches must be filtered (ConfigureNotify with unchanged dimensions). |

Today the platform layer carries a single `live_resizing: bool`
flag on each `Resized` event (`frankenterm/window/src/lib.rs:167`).
That isn't rich enough for sub-epic 2 — the renderer needs explicit
`Begin` / `Resizing` / `End` transitions plus recovery semantics
for the failure modes above.

This bead lands the **platform-agnostic state machine** that
encodes all of the above as pure logic. Per-platform recorders
(touching `os/macos/window.rs`, `os/wayland/window.rs`,
`os/x11/window.rs`) feed events into the machine and consume its
transitions. The machine plus its always-on regression net is the
foundation; the per-platform integrations are follow-on beads.

## State diagram

```text
   Idle ──BeginSignal──▶ ResizeBegin ──Configure──▶ Resizing ──EndSignal──▶ ResizeEnd ──▶ Idle
                                          ▲             │
                                          └─Configure───┘
                                                        │
                                                        ├─ MouseUpDuringResize ─▶ ResizeEnd
                                                        └─ Watchdog (5s no events) ─▶ ResizeEnd
```

Projected onto `(Idle → Begin → Resizing* → End → Idle)` the
diagram is acyclic. Every `ResizeBegin` is followed by exactly one
`ResizeEnd` (forced by the watchdog if the platform skips it). The
`assert_state_diagram_acyclic` invariant in the fixture pins this
across all goldens; the `adversarial_recovery_returns_to_idle`
proptest (256 random event sequences) extends the proof to every
reachable schedule.

## Failure-mode handling

Each per-platform failure mode is handled as a specific transition
source:

| Failure mode | Handler | Source tag |
| --- | --- | --- |
| macOS skipped DidEnd | `MouseUpDuringResize` event from cocoa correlation | `MouseUpRecovery` |
| Wayland configure storm (>100 in 100ms) | `classify_configure` returns `CoalesceDecision::Coalesce`; transition log stays clean, `coalesced_total` counter ticks | (no transition emitted) |
| X11 ConfigureNotify burst (no `_NET_WM_STATE_LIVE_RESIZE`) | First Configure during `Idle` synthesizes a `ResizeBegin` if dimensions changed | `ConfigureBurstSynthesizedBegin` |
| X11 fake-positive (workspace switch) | Configure with unchanged dimensions during `Idle` returns `None` | (no transition emitted) |
| Stuck-in-Resizing | `WatchdogTick` event compares its ts against `most_recent_activity_ts`; if Δ ≥ 5s, force End | `WatchdogForcedEnd` |

The fixture's per-platform synthetic streams exercise every
failure mode + recovery path and pin the resulting transition logs
as goldens.

## Headline correctness rules (proven by the fixture)

1. **State-diagram acyclicity.** Projected onto
   `(Idle → Begin → Resizing* → End → Idle)`, no cycle. Every
   `ResizeBegin` is paired with exactly one `ResizeEnd`.
2. **Adversarial recovery.** Any prefix of arbitrary events
   followed by a 5+s `WatchdogTick` returns the machine to `Idle`
   within one additional tick.
3. **Timestamp monotonicity.** Transition log timestamps are
   non-decreasing.
4. **Counter monotonicity.** `transitions_total >=
   watchdog_forced_ends_total + mouse_up_recoveries_total`.
5. **JSONL serde stability.** Transition logs round-trip through
   `serde_json` identity.
6. **Fake-positive immunity.** A Configure with unchanged
   dimensions during `Idle` does not transition.

## Watchdog correctness

The watchdog logic has one subtle invariant the fixture caught
during development:

> A `WatchdogTick` event MUST NOT reset the activity clock.

A naive implementation that updates "last event timestamp" on
every step would let a 5-second burst of silent watchdog ticks
silently reset the timeout — defeating the whole purpose.

The fix: the machine tracks `last_activity_ts_ms` separately and
updates it ONLY on non-`WatchdogTick` events. The
`adversarial_recovery_returns_to_idle` proptest pins this across
all reachable schedules.

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| `LiveResizeState` plumbed end-to-end | ✓ State machine shipped; integration is follow-on |
| Renderer receives state changes via event channel | ⏳ Integration bead (touches GUI event loop) |
| Stuck-in-Resizing watchdog (5s) | ✓ Enforced by `WATCHDOG_TIMEOUT_MS` + `WatchdogTick` |
| macOS skipped-DidEnd recovery | ✓ `MouseUpDuringResize` source |
| Wayland configure-storm coalescing | ✓ `CoalesceDecision::Coalesce` + counter |
| X11 ConfigureNotify dimension-change heuristic | ✓ `ConfigureBurstSynthesizedBegin` + fake-positive filter |
| Per-platform integration test | ✓ Fixture's synthetic streams (real-platform recorders are follow-on) |
| Structured logs reviewable post-hoc | ✓ JSONL goldens at `tests/live_resize/golden/<platform>-<scenario>.jsonl` |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`.
- **Downstream consumers:**
  - `ft-mpc9b.2.2` (render-loop draft mode) — reads
    `LiveResizeState::is_draft_mode()`.
  - `ft-mpc9b.2.3` (incremental reflow) — reads transitions to
    decide when to flip the reflow algorithm.
- **Watchdog primitive:** when the integration bead lands, the
  watchdog timer should use `runtime_async::sleep_with_cx`
  (asupersync per AGENTS.md), per the bead's
  `BR-RC-RUNTIME-SEMANTICS.G14` cross-link.
- **`ft doctor`:** `LiveResizeHealth` mirrors the
  `AtlasStabilityHealth` / `TripleBufferHealth` shape so the
  doctor surface can render all three side-by-side.

## Out of scope (follow-on beads)

- macOS recorder: `os/macos/window.rs` already has
  `live_resizing: bool` plumbing; integration adds a typed event
  emitter feeding into a singleton `LiveResizeStateMachine`.
- Wayland recorder: similar; needs reading the `xdg_toplevel`
  state into `BeginSignal`/`EndSignal`.
- X11 recorder: needs the `_NET_WM_STATE_LIVE_RESIZE` atom probe
  + ConfigureNotify dimension-tracking.
- Watchdog tick driver: `runtime_async::sleep_with_cx` loop
  emitting `WatchdogTick` events at ~100ms cadence.
- `ft doctor` rendering of `LiveResizeHealth`.
