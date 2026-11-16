# Render-Loop Draft Mode

**Bead:** [BR-TERM-EMULATOR-UPLIFT.2.2] / `ft-mpc9b.2.2`
**Sub-epic:** 2 — Live-Resize Fast Path

## Why this exists

Even with sub-epic 1's stable atlas (`ft-mpc9b.1.1`) and incremental
reflow (`ft-mpc9b.2.3`), painting a 200-pane fleet at full
fidelity during a drag gesture is too expensive on integrated GPUs
and battery-saving mode. The fix is the ghostty/rio pattern:
**drop to a low-fidelity render path while a gesture is active**,
then snap back to full quality on release.

This module ships the **policy layer** — `RenderQuality` enum,
`DraftModeFeatureFlags`, `DraftModeDriver` — that maps the
live-resize state machine's transitions onto a per-frame quality
choice. The actual rendering changes (skipping SDF / ligatures /
subpixel AA in the renderer's hot paths) live in the GUI
integration bead that consumes this contract.

## What's disabled in Draft

The bead enumerates 8 cosmetic features that Draft turns off:

| Feature | Standard | Fancy | Draft |
| --- | --- | --- | --- |
| SDF glyphs | ✓ | ✓ | ✗ |
| Ligature shaping (HarfBuzz / OpenType) | ✓ | ✓ | ✗ |
| Italic synthesis from non-italic fonts | ✓ | ✓ | ✗ |
| Subpixel anti-aliasing | ✓ | ✓ | ✗ |
| Fancy underlines (curly / dotted / double) | ✓ | ✓ | ✗ |
| Pane border decorations (shadows / glows) | ✓ | ✓ | ✗ |
| Focus blur effect | ✗ | ✓ | ✗ |
| Background-image scaling | ✓ | ✓ | ✗ |

The `cosmetic_feature_count()` accessor returns 7/8/0 for the
three qualities — useful for the renderer's per-frame budget
allocator.

## Three independence rules (DO NOT BREAK)

Three observable behaviors MUST be **independent** of
`RenderQuality`. The bead's "DO NOT BREAK" contract:

1. **Accessibility tree updates fire regardless.**
   `DraftModeFeatureFlags::a11y_tree_update` is `true` for every
   quality. Cross-link `ft-mpc9b.10.1`. A blind operator running
   VoiceOver / Orca / Narrator MUST receive announcements during
   resize even though the visual frame is in Draft.
2. **Color profile honored regardless.**
   `DraftModeFeatureFlags::color_profile` is `true` for every
   quality. Cross-link `ft-mpc9b.10.3`. True-color cells render at
   the correct gamut even in Draft; only AA / decoration is dropped.
3. **IME caret update fires regardless.**
   `DraftModeFeatureFlags::ime_caret_anchor` is `true` for every
   quality. Cross-link `ft-mpc9b.10.2`. The composition window
   anchors to the caret cell across resize.

The fixture's
`every_quality_must_dispatch_a11y_tree_updates` /
`every_quality_must_apply_color_profile` /
`every_quality_must_dispatch_ime_caret_anchor` tests assert these
across `RenderQuality::ALL` so a future variant added without
preserving the contract fails the lane.

## Snap-back guarantee

A resize gesture always ends with **exactly one Standard frame**:

```text
   Idle ─Standard─▶ Resizing (Draft × N) ─ResizeEnd─▶ Standard (snap-back) ─Idle─▶ steady_state
                                                          │
                                                          └── snap-back is ALWAYS Standard,
                                                              even if steady_state is Fancy
```

The snap-back is Standard (not Fancy) regardless of the steady-state
default — the user sees a guaranteed-correct reference frame
after the drag. Fancy effects re-engage on the *next* Idle frame.

If the integration layer skips the `ResizeEnd` tick (e.g., the
GUI's render loop polls Idle directly after the live-resize state
machine's auto-clear from `ResizeEnd → Idle`), the `DraftModeDriver`
**synthesizes** the snap-back on the first Idle frame. The
`skipped_resize_end_synthesizes_snap_back_on_next_idle` test
pins this; the proptest property
`snap_back_count_is_bounded_by_gesture_count` proves the
synthesis never produces extra snap-backs.

## Quality-flag plumbing

The bead's plumbing notes apply to the GUI integration bead:

- `glyphcache.rs` queries `DraftModeFeatureFlags::sdf_glyphs` /
  `ligature_shaping` / `italic_synthesis` to skip the relevant
  rasterization paths.
- `quad.rs` queries `fancy_underlines` /
  `pane_border_decorations` for simpler vertex generation.
- The shader uniform receives `subpixel_aa` as a uniform flag.
- `glyphcache.rs::snapshot_atlas_version` (shipped in
  `ft-mpc9b.1.1`) and the IME caret update path
  (`ft-mpc9b.10.2`) run unconditionally — they're in the
  independence-rule set.

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| `RenderQuality` enum + plumbing | ✓ Module shipped; integration is follow-on |
| Renderer switches to Draft on Resizing | ✓ Driver enforces; integration consumes |
| Renderer switches back on ResizeEnd | ✓ Driver snap-back guarantee |
| Snap-back as one full-quality repaint | ✓ Pinned via tests + proptest |
| A11Y tree updates fire regardless | ✓ Independence-rule test |
| Color profile applies regardless | ✓ Independence-rule test |
| IME caret-anchor regardless | ✓ Independence-rule test |
| Snap-back SSIM ≥ 0.999 vs reference | ⏳ GPU-integration bead |
| 60 FPS sustained during gesture (200-pane fleet) | ⏳ GPU-integration bead |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`.
- **Upstream consumer:** `live_resize` state machine
  (`ft-mpc9b.2.1`) — its `LiveResizeState::is_draft_mode()`
  and `DraftModeDriver::pick(state)` are the wiring point.
- **Downstream consumer:** GUI integration bead — touches
  `crates/frankenterm-gui/src/glyphcache.rs`,
  `quad.rs`, `shader.wgsl`, plus the paint-loop's per-frame
  driver tick. The integration bead is filed separately;
  this module is the contract it implements against.
- **`RenderQuality` re-export:** `frankenterm_core::ime_caret`
  re-exports `RenderQuality` for backward compatibility with
  the `ft-mpc9b.10.2` fixture (forward-looking declaration).
  The re-export is a `pub use`; serialized JSONL strings are
  identical (`snake_case`), so existing goldens stay valid.
