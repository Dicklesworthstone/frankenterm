# IME Composition-Window Caret-Anchor Audit

**Bead:** [BR-TERM-EMULATOR-UPLIFT.A11Y.2] / `ft-mpc9b.10.2`

IME (Input Method Editor) composition windows must track the caret
position. Pinyin / Japanese / Korean / Vietnamese users see a
floating composition window that anchors to where they're typing. A
renderer that drops or skips caret-position updates — during draft-
quality frames, live-resize gestures, or idle wake-up — leaves the
IME window stranded at a stale position. Visual screenshots look
identical; only an IME user notices the broken anchor.

## Headline finding

| Concern                                  | Today                                       | Gap                                         |
| ---------------------------------------- | ------------------------------------------- | ------------------------------------------- |
| Pure caret-rect math                     | Inline in `crates/frankenterm-gui/src/termwindow/mod.rs::update_text_cursor` (~25 lines). | Not unit-testable — required spinning a real GPU/window. **Closed by this bead** via the pure helper in `frankenterm_core::ime_caret`. |
| `set_text_cursor_position` call site     | Once per `paint_pass` (`render/paint.rs:272`). | Painted-frame-coupled: a skipped frame skips the IME update. |
| macOS dispatch                           | `set_text_cursor_position` writes the inner field, then calls `invalidateCharacterCoordinates` — the IME re-asks via `firstRectForCharacterRange`. | Lazy refresh, but only triggers when *something* prods the IME. A draft-quality burst that elides paints elides the prod. |
| X11 dispatch                             | Cell-rect dedup at `os/x11/window.rs:1837` (`if self.last_cursor_position == cursor`). Otherwise `update_ime_position` calls `ime.update_pos` with window-relative coords. | Dedup misses: window moved on screen (XIM caches stale position), window resized (cell rect collides post-resize), idle wake-up. |
| Wayland dispatch                         | Cell-rect dedup at `os/wayland/window.rs:1108` (`if self.text_cursor.map(|prior| prior != rect)`). Otherwise `set_cursor_rectangle + commit` to text-input-v3. | Same dedup gaps as X11. |
| RenderQuality coverage                   | The `Standard` / `Fancy` / `Draft` enum from `ft-mpc9b.2.2` does not exist yet. | The IME contract MUST guarantee dispatch in every quality before 2.2 lands; this bead pins that as a forward-looking invariant. |

**Bottom line.** Today the renderer's IME caret-anchor path works in
the steady state but has three latent failure modes the integration
beads under `ft-mpc9b.2.2` and `ft-mpc9b.5.3` will trigger:

1. **Draft-quality elide.** A draft frame that doesn't call
   `paint_pass` skips `update_text_cursor`.
2. **Window-state drift.** The X11 / Wayland dedups key on cell-rect
   alone; window moves, resizes, and idle wake-up all leave the IME
   with a stale anchor.
3. **Untested math.** The caret-rect computation is GUI-coupled and
   currently has no unit test coverage.

## Code citations

### `crates/frankenterm-gui/src/termwindow/mod.rs`

- `update_text_cursor` (line ~2251). Inline math computes a
  `Rect::new(...)` from pane cursor + tab-bar + padding +
  cell-size, then calls `Window::set_text_cursor_position`. **This
  bead** routes the math through the new pure
  `frankenterm_core::ime_caret::compute_caret_anchor_rect` helper.

### `crates/frankenterm-gui/src/termwindow/render/paint.rs`

- `paint_pass` (line ~272). The only caller of
  `update_text_cursor`. Coupling the IME update to the paint loop
  is what makes draft-quality / idle-wake-up cases fail.

### `frankenterm/window/src/os/macos/window.rs`

- `set_text_cursor_position` (line 1360). Writes `inner.text_cursor_position`,
  calls `invalidateCharacterCoordinates`. The IME reads via
  `firstRectForCharacterRange` (line 2189).

### `frankenterm/window/src/os/x11/window.rs`

- `set_text_cursor_position` (line 1836). **Cell-rect dedup at
  line 1837** — the audit's headline X11 gap.

### `frankenterm/window/src/os/wayland/window.rs`

- `set_text_cursor_position` (line 1099). **Cell-rect dedup at
  line 1108** — same gap as X11.

## Initial fix shipped with this bead

This bead ships the **observability + math foundation**, not the
per-platform dedup-bypass:

1. **Core module** at `crates/frankenterm-core/src/ime_caret.rs`:
   - `CaretAnchorRect` (window-relative caret rect).
   - `CaretGeometry` (the minimal pane geometry input).
   - `compute_caret_anchor_rect()` — the pure caret math.
   - `RenderQuality` enum (forward-looking; populated by
     `ft-mpc9b.2.2`) with `must_dispatch_caret_update()` always
     true.
   - `ImePlatform` (macOS / Wayland / X11 / Synthetic) with an
     `is_wired()` honesty sentinel.
   - `ImeScenario` (typing / draft-quality burst / live-resize /
     idle wake-up / focus change).
   - `ImeUpdate` (one row of `tests/ime/logs/<platform>-<scenario>.jsonl`).
   - `ImeDispatchState` + `should_dispatch_after_state_change` —
     the corrected predicate that catches window-move, resize,
     quality-flip, and idle-wake-up deltas the platform dedups
     miss.
   - 13 unit tests pinning the math and predicates.

2. **GUI rewire** in `crates/frankenterm-gui/src/termwindow/mod.rs::update_text_cursor`:
   - Routes the inline math through `compute_caret_anchor_rect`.
   - Behavior is byte-for-byte identical (same i64 math, same
     clamp semantics) so no platform integration regresses.

3. **Regression fixture** at
   `crates/frankenterm-core/tests/ime_caret_anchor_fixture.rs`:
   - Per-scenario golden snapshots with `FT_IME_BLESS=1`
     deliberate-bless flow.
   - 4 proptest properties (256 cases each):
     `compute_caret_anchor_rect` totality, y-monotonic in row,
     x-monotonic in col, tab-bar offsets y one-for-one.
   - `corrected_predicate_dominates_cell_rect_dedup` proves
     `should_dispatch_after_state_change` strictly subsumes the
     existing X11 / Wayland dedups.
   - `only_synthetic_platform_is_wired_today` sentinel that fires
     when an integration lands without flipping `is_wired()`.

## Closure plan

The follow-on integration work that fills the gap above lives in
sibling beads under `ft-mpc9b.10`:

- **macOS NSTextInputClient recorder** (one bead): wire a
  `Synthetic`-style recorder that captures
  `firstRectForCharacterRange` calls; verify the contract
  against `tests/ime/golden/macos-<scenario>.jsonl`.
- **Linux text-input-v3 recorder** (one bead): same for Wayland.
- **Linux XIM recorder** (one bead): same for X11.
- **X11 / Wayland dedup-bypass** (one bead): replace the cell-rect
  dedups in `os/x11/window.rs:1837` and
  `os/wayland/window.rs:1108` with calls to
  `should_dispatch_after_state_change`. The corrected predicate
  needs `ImeDispatchState` plumbed through; the integration bead
  owns that.
- **`ft-mpc9b.2.2` integration**: when the `RenderQuality::Draft`
  path lands, the regression fixture's
  `must_dispatch_caret_update()` invariant fires automatically if
  the new path elides the IME update — no extra wiring needed.
- **`ft-mpc9b.5.3` integration**: same for the idle frame-rate
  dropdown; `was_idle` transitions are already covered by
  `should_dispatch_after_state_change`.

## Acceptance signals shipped

- ✅ IME caret-rect math extracted into a unit-testable pure helper.
- ✅ Per-RenderQuality dispatch invariant pinned (forward-looking).
- ✅ Corrected dispatch predicate dominates the platform dedups.
- ✅ Per-scenario goldens committed for the synthetic recorder.
- ✅ Sentinel test fires when an integration lands.
- ⏳ Per-platform recorders (macOS / Wayland / X11) — follow-on
  beads.
- ⏳ Live-resize / idle-wake-up E2E — follow-on beads.
