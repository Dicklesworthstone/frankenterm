# Incremental Terminal-Grid Reflow

**Bead:** [BR-TERM-EMULATOR-UPLIFT.2.3] / `ft-mpc9b.2.3`
**Sub-epic:** 2 — Live-Resize Fast Path

## Why this exists

The current resize path in `frankenterm/term/src/screen.rs::rewrap_lines`
(line 1625) is logical-line-cache-based: it computes a layout
signature, hits/misses a cache, and on miss does an O(N) rewrap
of every logical line. For a 200-pane fleet with 10k-line
scrollbacks that's enough work to feel during a drag.

The bead asks for an **O(damage)** algorithm: a per-line predicate
that skips re-lining when the line wraps identically under both
the old and new widths. A line that fits in both — the typical
case during a typing-driven resize — never re-emits cells.

This module ships the **pure-function reflow algorithm** with full
Unicode/wide-char/ZWJ correctness. Integration into
`screen.rs::rewrap_lines` is the follow-on bead.

## Algorithm

Per-line wrap is a single pass:

```text
for cell in line:
  if cell is joiner: continue          # combining mark / ZWJ
  if col + cell.width > width:         # wrap before
    breaks.push(cell_index)
    col = cell.width
  else:
    col += cell.width
```

Two correctness guarantees:

1. **Wide-cell safety.** A 2-cell glyph that would land at column
   `width-1` wraps to the next row entirely; the algorithm never
   produces a row whose visual width exceeds the wrap width.
2. **Joiner attachment.** Combining marks, ZWJ, variation
   selectors travel with their preceding base cell — even at a
   wrap boundary. The fixture's
   `combining_mark_travels_with_base_across_wrap` and
   `emoji_zwj_family_stays_intact_under_wrap` tests pin this.

## The O(damage) skip predicate

`should_skip_reflow(cells, old_width, new_width) -> bool` returns
`true` iff the line wraps identically under both widths. The
integration layer's per-frame loop:

```rust
for line in screen.lines:
    if should_skip_reflow(&line.cells, old_width, new_width):
        continue;  // line stays in its existing rows
    let new_rows = reflow_line(&line.cells, new_width);
    apply(new_rows);
```

Steady-state typing (most lines fit in both widths) reports
`ReflowHealth::skip_ratio() > 0.95`. The integration bead's
`ft doctor` rendering surfaces this counter.

## Correctness corpus

The regression fixture exercises the bead's required cases:

| Scenario | Test |
| --- | --- |
| ASCII basic wrap | `ascii_50_chars_wraps_correctly_at_width_30` |
| Wide cells (CJK) never split mid-cell | `cjk_never_splits_mid_cell_under_any_width` |
| Emoji ZWJ family stays intact | `emoji_zwj_family_stays_intact_under_wrap` |
| Combining marks travel with base | `combining_mark_travels_with_base_across_wrap` |
| Style preservation across wrap | `ascii_styled_cells_preserve_style_across_wrap` (lib) |
| Cursor remap round-trip | `cursor_remap_round_trips_under_identity_widths` |
| Cursor remap proportional | `cursor_remap_preserves_logical_position` |
| Skip predicate reflexive | `skip_predicate_reflexive` (proptest) |
| Skip predicate symmetric | `skip_predicate_symmetric` (proptest) |
| Reflow preserves cell count | `reflow_preserves_cell_count` (proptest) |
| Wide-cell rule under random inputs | `wide_cells_never_split_under_random_inputs` (proptest) |

Each proptest runs 256 random cases.

## Out of scope

- **BiDi reordering.** Cross-link `A11Y.4`. This module handles
  the LTR wrap algorithm; BiDi paragraph-level reordering is a
  post-processing pass the integration bead applies.
- **Persistent rope (HAMT/RRB).** The bead's optional
  alien-artifact alternative — evaluated separately in
  `ft-mpc9b.2.5`. The `Cell` representation here doesn't
  preclude it; the integration bead can swap the storage type
  without changing the algorithm.
- **`screen.rs::rewrap_lines` migration.** This is the follow-on
  integration bead. Mapping `frankenterm_term::Cell` onto
  `grid_reflow::Cell` is straightforward (cell width, joiner
  flag, opaque style hash); the rewrap call becomes a per-line
  loop with the skip predicate.
- **Bench targets** (1000-line < 5ms, 10k-line < 50ms). The
  pure-function algorithm is O(line_length) per line; achieving
  the targets requires the integration bead's per-line skip
  loop. The fixture's invariant tests are deterministic;
  benchmarking is a follow-on under the existing bench
  infrastructure.

## Bead acceptance status

| Acceptance item | Status |
| --- | --- |
| Incremental algorithm replaces O(N) | ✓ Pure algorithm shipped; integration is follow-on |
| 1000-line bench < 5ms | ⏳ Integration bead (needs `screen.rs` migration) |
| 10k-line bench < 50ms | ⏳ Integration bead |
| Reflow regression suite passes | ✓ 21 lib + 17 fixture tests pass |
| CJK + emoji + ZWJ correctness | ✓ Per-scenario tests + proptest |
| BiDi correctness | ⏳ Cross-link `A11Y.4` |
| Cursor-remap correctness | ✓ `remap_cursor` + tests |
| Persistent-rope evaluation | ⏳ Cross-link `ft-mpc9b.2.5` |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`.
- **Upstream consumer:** `live_resize` state machine
  (`ft-mpc9b.2.1`) — when the machine reports `Resizing`, the
  integration layer calls `should_skip_reflow` per line; when
  it reports `ResizeEnd`, the snap-back paint pass commits the
  reflowed rows.
- **Downstream consumer:** `ft-mpc9b.1.2` (per-line dirty
  bitmap) — reflowed rows mark themselves dirty so the
  per-line dirty path picks them up.
