# Differential Render Oracle (ftui ↔ ratatui Parity)

**Bead:** [BR-RC-CUTOVERS.G5.1] / `ft-35yac.1`
**Status:** Foundation slice shipped. Contract layer
(`RenderFrame` / `RenderCell` / `FrameDiff` / `KeymapAction` /
`EventScript` / `OracleHealth`) + comparator + synthesized
event corpus + property-test scaffold (256 cases × 6
properties) all live. The retained backend-driver slice now
calls the actual ratatui app/views path and the ftui
`WaModel::view` path with matched deterministic state, then
normalizes both buffers into `RenderFrame`; the current driver
evidence reports a backend divergence rather than a clean
parity pass. vhs/asciinema-derived corpora is sub-bead
`ft-35yac.1.1`; GPU-renderer parity is sub-bead
`ft-35yac.1.2`.

## Why this matters

`crates/frankenterm-core/src/tui/` ships **27,245 LOC** across
two backends compiled side-by-side:

- Legacy: `tui/views.rs` (151k bytes) — the ratatui stack.
- Migration target: `tui/ftui_backend.rs` (275k bytes) — the
  ftui stack.

The rollout (Stages 1–3 in `tui/rollout.rs`) keeps both alive
until ratatui can be deleted. Until that deletion lands, every
render path needs a parity oracle — a mechanism for asserting
that **byte-identical render frames** come out of both backends
under the **same input event stream**.

The bead's headline rule:

> Build harness driving both backends with same input event
> stream. Assert byte-identical render frames; failures emit
> insta-style diffs for triage.

## Artifacts

| Artifact | Location |
|---|---|
| Contract module | `crates/frankenterm-core/src/tui_parity_oracle.rs` |
| Property-test harness | `crates/frankenterm-core/tests/tui_parity_oracle.rs` |
| This audit doc | `docs/security/tui-render-parity.md` |

## Type hierarchy

```text
RenderFrame                      cell grid produced by either backend
  ├─ width, height : u16
  └─ cells : Vec<RenderCell>     row-major

RenderCell                       backend-agnostic single cell
  ├─ ch        : char
  ├─ fg, bg    : Rgba            32-bit RGBA, both backends project
  ├─ bold/italic/underline/reverse : bool
  └─ continuation : bool         trailing column of a wide glyph

FrameDiff                        result of compute_diff(left, right)
  ├─ dimension_mismatch : bool   true ⇒ cells empty
  ├─ left_dim, right_dim
  └─ cells : Vec<CellDiff>       sorted by (row, col)

KeymapAction                     mirror of tui::keymap::Action
                                 (32 variants matching the canonical table)

EventScript                      sequence of KeymapActions
  ├─ name, rationale
  ├─ initial_view : u8           1..=7
  ├─ width, height : u16
  └─ actions : Vec<KeymapAction>

OracleHealth                     ft doctor counter snapshot
                                 (matches *Health pattern of sibling fixtures)
```

## The comparator

`compute_diff(left, right) -> FrameDiff` is pure-logic, free of
allocation beyond the diff's own `Vec`. Properties:

- **Reflexivity:** `compute_diff(f, f).is_clean()` for any
  well-shaped frame `f`.
- **Symmetry on count:** `diff(a, b)` and `diff(b, a)` report
  the same divergent-cell count.
- **Cell-wise correctness:** the divergent-cell count equals
  the manual count of structurally-unequal `(row, col)`
  positions.
- **Dimension-strict:** different widths/heights ⇒
  `dimension_mismatch = true` AND `cells = []`. The
  comparator does not attempt cross-dim alignment (that's a
  failure mode — a backend regressed on layout-arithmetic).
- **Continuation-flag insensitive:** wide-glyph trailing-cell
  flags don't affect equality. Backends with different
  conventions for the trailing column don't false-positive.

`FrameDiff::render_summary(max_lines)` produces an insta-style
human-readable diff:

```text
(0,5): ' ' Rgba { r: 204, g: 204, b: 204, a: 255 }/Rgba { r: 0, g: 0, b: 0, a: 255 } → 'X' Rgba { r: 255, g: 0, b: 0, a: 255 }/Rgba { r: 0, g: 0, b: 0, a: 255 }
(0,6): ' ' Rgba { r: 204, g: 204, b: 204, a: 255 }/Rgba { r: 0, g: 0, b: 0, a: 255 } → 'Y' ...
... and 17 more divergent cells
```

## Keymap action coverage

`KeymapAction` mirrors `crate::tui::keymap::Action` — **32
variants** covering: global (Quit/ShowHelp/Refresh/NextTab/
PrevTab/GoToView/ListNext/ListPrev), filter
(FilterAppendChar/FilterDeleteChar/FilterClear), Panes-view
toggles (5 cycle/toggle actions), Events digit filter, Triage
(primary/mute/expand/numbered), History undoable filter,
Search (5 saved-search actions), Timeline (zoom in/out, scroll
left/right).

Coverage analysis (`every_keymap_action_kind_appears_in_corpus_or_is_explicitly_omitted`):

- **30 of 32** kinds covered by the synthesized corpus.
- **2 explicitly omitted:** `PrevTab` (covered indirectly by
  NextTab + symmetry) and `ApplyRulesetProfile` (rare action;
  hits the same code path as `CycleRulesetProfile`).
- The proptest sweep (`arb_keymap_action`) generates ALL 32
  kinds with uniform-ish probability, so the property tests
  cover what the synthesized corpus omits.

## Synthesized event corpus

`synthesized_event_corpus()` ships **12 hand-curated event
scripts** at multiple frame dimensions:

| Script | Dimensions | Targets |
|---|---|---|
| `smoke_quit_from_home` | 80×24 | minimal — quit from default view |
| `tab_cycle_all_views` | 80×24 | NextTab through every view |
| `goto_view_jumps` | 80×24 | direct jump to all 8 views |
| `panes_filter_toggle` | 100×30 | filter toggles + cycles in Panes view |
| `search_filter_text_entry` | 80×24 | FilterAppend/Delete/Clear sequence |
| `events_digit_filter` | 80×24 | EventsFilterDigit cycle |
| `triage_numbered_actions` | 80×24 | TriageNumberedAction(1..9) + primary |
| `history_undoable_filter` | 80×24 | ToggleUndoableOnly + list nav |
| `search_saved_cycle` | 100×30 | SearchNext/PrevSaved + Run/Toggle/Execute |
| `timeline_zoom_scroll` | 120×30 | Zoom in/out + horizontal scroll |
| `show_help_overlay_then_dismiss` | 80×24 | ShowHelp triggers a modal — overlay parity |
| `small_terminal_dimensions` | **40×12** | narrow-frame layout paths |

Every script terminates in `KeymapAction::Quit` (asserted by
`corpus_ends_in_quit`) so the integration driver has a clean
shutdown signal.

## Property-test scaffold

`tests/tui_parity_oracle.rs` runs **6 proptest properties at
256 cases each** (~1,500 random schedules per CI run):

1. `diff_self_is_clean_property` — reflexivity.
2. `diff_is_symmetric_in_count_property` — symmetry on count
   AND `dimension_mismatch` flag.
3. `diff_count_matches_manual_count` — cell-wise correctness
   (the comparator's count matches a brute-force scan).
4. `dimension_mismatch_flag_correctness` — flag fires iff
   dimensions differ.
5. `triangle_inequality_on_diverged_cells` — cell-set
   triangle inequality `|a⊕c| ≤ |a⊕b| + |b⊕c|`. A flush-
   coalescing optimization that violated this would indicate
   a comparator bug.
6. `continuation_flag_does_not_affect_diff` — wide-glyph
   trailing flag is invisible to the comparator.

Plus 2 proptests on the input alphabet:

7. `keymap_action_kind_is_stable` — every random
   `KeymapAction` projects to a stable kind and round-trips
   through serde.
8. `event_script_serde_roundtrip` — random `EventScript`s
   round-trip stably.

## Backend-driver status

The retained rollout-gated driver test now:

- Constructs a deterministic `QueryClient` fixture shared by
  both backends.
- Drives `app.rs`/`views.rs` through the real ratatui
  `App::render` path.
- Drives `ftui_backend.rs` through the real `WaModel::view`
  path.
- Captures both buffers as normalized `RenderFrame`s via
  `render_frame_from_ratatui_buffer` and
  `render_frame_from_ftui_frame`.
- Calls `compute_diff(ratatui_frame, ftui_frame)` and asserts
  any divergence emits an actionable insta-style summary plus
  glyph-vs-style and top/body/last-row buckets.

This changes the honest blocker from "oracle unavailable" to
"backend-driver divergence." The remaining integration work is
not availability of a driver; it is making the retained driver
clean across the full script/corpus set and then publishing the
release evidence.

## What the foundation slice does NOT do

- Does not yet drive every `EventScript` step through both
  event loops. The current retained driver covers deterministic
  Home and Panes state fixtures; the follow-on extends that to
  the whole `synthesized_event_corpus()` action stream.
- Does not yet assert `is_clean()` for all ratatui-vs-ftui
  frames. Current status is an explicit divergence blocker.
- Does not record vhs/asciinema corpora — that's
  `ft-35yac.1.1`.
- Does not assert GPU-renderer parity — that's
  `ft-35yac.1.2`.
- Does not publish per-release JSON artifact — depends on
  `ft-syqcz.1` schema bead.

## CI gate (when integration lands)

> ratatui is the **reference oracle**, not a deprecated stack
> to delete.

Per the bead's action #5: ratatui is treated as the trusted
reference until ratatui-deletion at FTUI-09.5. Divergence
between ftui and ratatui frames is **always** an ftui bug; CI
fails with the insta-style diff dumped to the failure log.

The synthesized corpus runs as part of every PR (~50ms
runtime). The vhs/asciinema corpora (when vendored at sub-bead
`ft-35yac.1.1`) extend the CI run; the heavy lane runs the
~1,500 proptest properties at 4× scale (~6,000 schedules per
release).

## Re-running

```bash
# Library tests (comparator + corpus shape + health rollup +
# JSONL roundtrip):
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --lib tui_parity_oracle:: \
    --features asupersync-runtime --no-default-features
# → 20 passed

# Property-test harness (6+2 props × 256 cases each, plus
# corpus invariants):
cargo test -p frankenterm-core --test tui_parity_oracle \
    --features asupersync-runtime --no-default-features
# → 11 passed; ~1,500 schedules per run
```

## Bead acceptance status

| Item | Status |
|---|---|
| Harness exists, runs in CI | Partial: regression net is always-on; the retained backend driver now reaches real ratatui and ftui renderers and is invoked by the SSIM release gate |
| Reports zero divergence on parity corpus | Partial: degenerate self-compare stays clean across 256 random frames per property; retained ratatui-vs-ftui driver is clean for the deterministic Home/Panes cases, while the full retained release corpus remains pending |
| Property-based parity using keymap | ✓ (32 KeymapAction kinds; full alphabet swept in proptest) |
| ratatui as reference oracle | ✓ (documented as "always an ftui bug" semantics) |
| vhs/asciinema corpus from real sessions | ⏳ sub-bead `ft-35yac.1.1` |
| Headless GPU-renderer parity | ✓ `scripts/test-gpu-harness.sh` runs the headless GPU harness and emits `render-parity-gpu.json`; CI uploads the run directory from the macOS Metal gate and Linux llvmpipe pilot |
| Per-release render-parity JSON | Partial: GPU visual adjunct is attested by `docs/attestations/tui/render-parity-gpu.json`; the full ratatui<->ftui byte-level report remains `ft-35yac.2` |
| Backend driver wiring | Partial: retained driver reaches both real backends and reports clean deterministic Home/Panes frames; full clean script/corpus run remains pending |

## Cross-references

- **Production TUI:** `crates/frankenterm-core/src/tui/`
  (27k LOC; `views.rs` is ratatui, `ftui_backend.rs` is
  the migration target).
- **Canonical keymap:** `crates/frankenterm-core/src/tui/keymap.rs`
  (32 `Action` variants, `KEYMAP` table); the
  `KeymapAction` mirror in this oracle MUST stay synchronized.
- **Rollout policy:** `crates/frankenterm-core/src/tui/rollout.rs`
  (Stages 1–3 + `FT_TUI_BACKEND` env override).
- **Sibling foundation fixtures** (same session pattern,
  `*Health` + JSONL + property-test harness):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`, `redactor_coverage_matrix`.
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`).
