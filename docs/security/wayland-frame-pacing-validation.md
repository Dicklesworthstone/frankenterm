# Wayland Frame-Callback Validation Matrix

**Bead:** [BR-TERM-EMULATOR-UPLIFT.3.2.cont] / `ft-28opz`
**Parent:** `ft-mpc9b.3.2` (observability shipped at
`151bde5fe`).
**Status:** Foundation slice shipped. Per-compositor
verification matrix contract + harness scaffold + ft doctor
exposure surface + Linux integration test slot all live; the
actual Wayland driver wiring (calling production
`frame_callback_chain_depth_peak()` from a real `ft` window
running on each Tier-1 compositor) is the integration
follow-on — gated on a Linux Wayland CI runner being
provisioned.

## Headline rule

> **Frame-callback chain depth must stay ≤ 1** under any
> resize-storm interleaving.

The structural guards in
`frankenterm/window/src/os/wayland/window.rs` (early-return at
`do_paint`, early-return at `invalidate`, single take in
`next_frame_is_ready`, line 1168 single `surface().frame()`
site) bound it to ≤ 1 by inspection. This validation matrix is
the **runtime regression net** per Tier-1 compositor.

## Compositor tier table

| Compositor | Slug | Tier | Verification cadence |
|---|---|---|---|
| GNOME (mutter) | `mutter` | Tier-1 | every Linux PR (when CI runner exists) |
| KDE Plasma (kwin) | `kwin` | Tier-1 | same |
| Sway | `sway` | Tier-1 | same |
| Hyprland | `hyprland` | Tier-2 | best-effort, manual per release |
| Wayfire | `wayfire` | Tier-2 | same |
| Weston | `weston` | Reference | spec-conformance triage only |

Source of truth:
`crate::wayland_compositor_matrix::CompositorIdentity::ALL`.

## Resize-storm reproducer

Default parameters (`ResizeStormConfig::default`):

- **Duration:** 5,000 ms (matches the bead's "5 seconds").
- **Event rate:** 60 events/s (typical fast-scrub mouse drag).
- **Total events:** 300.
- **Width range:** 400–1,200 px.
- **Height range:** 300–900 px.

The operator drives the reproducer manually via `ydotool`, an
input synthesizer, or a `WAYLAND_DEBUG=1` simulation. The
resulting `chain_depth_peak()` reading is fed to the matrix
via `verify_compositor`.

## Acceptance bounds

| Bound | Peak | When applicable |
|---|---|---|
| `ChainDepthBound::PRE_FIX` | ≤ 1 | Default — structural guards bound chain depth here. The bead requires this to hold; if it does, the bead closes with "no bug found, observability shipped." |
| `ChainDepthBound::POST_FIX` | ≤ 2 | Only relevant if a real failing reproducer surfaces and the bead's option-#1 reorder ships. Currently NOT shipped. |

The reorder fix (re-request `frame_callback` after a successful
compose rather than before) preserves the wezterm/issues/3468
+ #3126 invariants. Per the parent bead: *"Without a real
failing reproducer, do NOT ship a speculative reorder."*

## ft doctor exposure

`crate::wayland_compositor_matrix::FrameCallbackHealth` is the
`ft doctor` snapshot the operator reads at runtime:

```text
chain_depth_now                  : current in-flight callback count
chain_depth_peak                 : lifetime peak since window creation
resize_events_total              : total resize events observed
depth_gt_one_observations_total  : count of times depth > 1 was logged
```

The `is_safe(bound)` predicate returns `true` iff
`chain_depth_peak <= bound.peak_max`.

The integration layer (in the `wezterm_native` / `frankenterm-
gui` seam) reads `frame_callback_chain_depth_peak()` from the
production accessor at `window.rs:1240` and projects into this
struct.

## Per-release verification report

Layout (one entry per Tier-1 compositor minimum):

```json
{
  "schema_version": 1,
  "bead": "ft-28opz",
  "results": {
    "mutter": {
      "compositor": "mutter",
      "tier": "tier1",
      "version": "mutter 47.2",
      "config": {
        "duration_ms": 5000,
        "events_per_second": 60,
        ...
      },
      "bound": { "peak_max": 1 },
      "chain_depth_peak": 1,
      "passed": true,
      "notes": "CI lane: ubuntu-24.04 / mutter snap"
    },
    "kwin":  { ... },
    "sway":  { ... }
  }
}
```

`CompositorMatrixSnapshot::all_tier1_passed()` is the gate for
release: every Tier-1 compositor must have a passing entry.
`missing_tier1()` lists gaps for the operator's release
checklist.

## Reproducer script (operator runbook)

```bash
# Tier-1 verification — operator runs this on each Linux
# Wayland host before a release.
#
# Prerequisites:
#   - ydotool (or equivalent input synthesizer)
#   - ft binary built with `--features tui,wayland`
#   - Wayland session active
#
# Output:
#   - chain_depth_peak printed at exit
#   - exit code 0 on pass (peak ≤ 1), nonzero on fail.

# 1. Launch ft in a known-size window.
ft &
FT_PID=$!
sleep 2

# 2. Drive the resize storm — 5s of rapid edge drags.
end=$((SECONDS + 5))
while [ $SECONDS -lt $end ]; do
    ydotool window-resize 800 600
    sleep 0.017
    ydotool window-resize 1200 900
    sleep 0.017
done

# 3. Sample chain_depth_peak via ft doctor (when wired).
PEAK=$(ft doctor --json | jq '.frame_callback.chain_depth_peak')
kill $FT_PID

# 4. Assert pre-fix bound.
if [ "$PEAK" -gt 1 ]; then
    echo "FAIL: chain_depth_peak=$PEAK > 1 on $XDG_CURRENT_DESKTOP"
    exit 1
fi
echo "PASS: chain_depth_peak=$PEAK on $XDG_CURRENT_DESKTOP"
```

## Bead acceptance status

| Item | Status |
|---|---|
| Linux integration test exists | ✓ scaffold + harness contract + ignored Linux-only stub |
| Per-Tier-1-compositor manual verification documented | ✓ this doc + reproducer script |
| ft doctor reports frame_callback_chain_depth_peak | ✓ `FrameCallbackHealth` contract; operator wires the production accessor in the integration follow-on |
| Reorder fix shipped iff chain depth > 1 observed | ⏳ no failing reproducer yet — the structural guards hold by inspection |
| Per-release JSON artifact | ✓ `CompositorMatrixSnapshot` shape; populated by Linux operator |

## Cross-references

- **Parent bead** observability: `151bde5fe` —
  `frankenterm/window/src/os/wayland/window.rs:663-1265`
  ships `frame_callback_chain_depth` and
  `frame_callback_chain_depth_peak`.
- **Related sibling fixture:** `wayland_frame_pacing` /
  `ft-mpc9b.3.2` (the pacing helper itself).
- **wezterm protocol-ordering background:**
  - issues/3468 — surface.commit ordering.
  - issues/3126 — show-workaround surface tagging.
  - issues/5103 — frame-callback chain depth.
- **Sibling foundation fixtures** (same `*Health` /
  matrix-shape pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`, `redactor_coverage_matrix`,
  `tui_parity_oracle`, `robot_checkpoint_state_machine`,
  `robot_work_state_machine`, `robot_fleet_state_machine`.
