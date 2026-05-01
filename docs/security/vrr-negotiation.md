# Variable Refresh Rate (VRR) Per-Frame Negotiation

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.2.1] / `ft-2okh0.2.1`
**Status:** Foundation slice shipped. Per-platform API
identity + display-capability + decision tree + negotiation-
outcome taxonomy + health snapshot + 24 lib tests all live;
production per-platform negotiation (Wayland
`presentation-time`, X11 `XPresent`, macOS `CADisplayLink`)
is the integration follow-on.

## Headline rule

> ft tells the compositor a **desired refresh rate per
> frame**. Idle → 30 Hz. Typing → 60 Hz. Live-resize → display
> max. Battery low → 30 Hz cap. Recording active → fixed
> rate. Massive battery savings without latency cost.

## Decision priority

`decide_request(inputs, display, vrr_disabled_when_recording)`
implements this fixed priority order:

1. **Recording active** + flag set → `RecordingDisablesVrr`
2. **Live-resize** in flight → `LiveResizeMax` (display max)
3. **Input wake-up** → `InputWakeUp` (display max for one frame)
4. **Animation active** → `AnimationFloor` (≥60 Hz)
5. **Battery state** caps the candidate (`BatteryLowCap` 30 Hz / `BatteryNormalCap` 60 Hz)
6. **Long idle streak** (≥120 frames) → `IdleFloor` (30 Hz)
7. **Default** → `PluggedIdleDefault` (60 Hz)

The display capability clamps the result to `[min_rate_hz,
max_rate_hz]` (FreeSync panels typically floor at 48 Hz).

## Per-platform API

| Platform | API | Identity slug |
|---|---|---|
| Wayland (mutter / kwin / sway / hyprland) | `wp_tearing_control_v1` + `presentation-time` | `wayland_presentation_time` |
| X11 | `XPresent` + Present extension | `x11_present` |
| macOS | `CADisplayLink.preferredFrameRateRange` | `macos_ca_display_link` |
| (no support) | fixed-rate Present | `unsupported` |

`VrrPlatformApi::supports_per_frame_rate()` filters out
`Unsupported`; the integration falls back to fixed-rate
Present without regression.

## "DO NOT BREAK" rules (encoded in the decision tree)

- **A11Y**: assistive-tech announcements stay event-driven —
  the predicate doesn't gate AT updates; OS-paint signals
  (cross-link `ft-458t7`) carry them through.
- **Idle wake-up**: arrival of an input event immediately
  bumps to display max for that frame —
  `inputs.input_event_arrived = true` overrides everything
  except recording.
- **Recording**: `vrr_disabled_when_recording = true` (default)
  forces fixed rate so OBS / screen-record cadence is stable.

## Telemetry

`VrrHealth` records:

- `display: DisplayCapability` — what the doctor probed.
- `vrr_active` — true once any negotiation has fired.
- `negotiations_total` — lifetime count.
- `mismatched_total` — count of `ClampedByCompositor` /
  `Failed` outcomes (the bead's
  `mismatched_negotiated_vs_actual_rate` indicator).
- `failed_total` — separate failed-negotiation counter.
- `rate_distribution` — histogram of negotiated Hz values.
- `reason_distribution` — histogram of `RequestReason`
  variants.

`is_safe()`: mismatch rate ≤ 5% AND failed ≤ 1% of total.

## Tests (24)

- All 4 platform APIs have distinct slugs.
- `clamp` round-trips: unclamped / below floor / above
  ceiling / unsupported.
- All 7 decision-priority paths covered with their named
  reason.
- Boundary cases: recording-flag on/off, input-wakeup beats
  long-idle, FreeSync floor clamps idle request, unsupported
  display returns max regardless of inputs.
- Mismatch predicate correctness (Honored / Clamped / Failed
  / FellBack).
- Health folding + 5%-mismatch is_safe boundary.

## Bead acceptance status

| Item | Status |
|---|---|
| Per-platform API identity enum | ✓ `VrrPlatformApi` |
| Display-capability probe shape | ✓ `DisplayCapability` |
| Per-frame decision tree | ✓ `decide_request` (pure logic, 7-priority) |
| Telemetry (rate distribution + mismatch counter) | ✓ `VrrHealth` |
| `vrr_disabled_when_recording` flag | ✓ wired into decision tree |
| Doctor probe at startup | ⏳ integration follow-on (per-platform code) |
| Per-frame negotiation calls (Wayland / X11 / macOS) | ⏳ integration follow-on |
| Doctor reports `vrr_supported`, `vrr_active`, distribution | ⏳ one-line projection from health snapshot |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Sibling: `ft-458t7` (`should_paint` predicate — same
  family of paint-policy contracts), `ft-mpc9b.5.3` (idle
  rate dropdown — VRR is the implementation surface),
  `ft-2okh0.7` (battery-aware FPS — VRR carries the chosen
  rate to the compositor).
- Sibling fixtures (this session): 30 prior `*Health`
  contract modules.
- Attestation: `ft-syqcz.1`.
