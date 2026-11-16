# Zellij muxer architecture analysis (wa-2dd4s.1)

This document is a deeper dive than `evidence/zellij/INVENTORY.md`: it focuses specifically on Zellij’s muxer/layout/resize system (tiled + floating panes), and maps the “steal this” ideas into concrete FrankenTerm/WezTerm work.

Upstream repo: `https://github.com/zellij-org/zellij`  
Local clone (gitignored): `legacy_zellij/`  

## Executive summary (what matters for FrankenTerm)

Zellij’s muxer innovations relevant to FrankenTerm fall into three buckets:

1. **Layouts as a first-class, shareable contract** (`zellij-utils`):
   - Layout parsing lives in `zellij-utils` so both client and server share the same schema and error messages.
   - Layouts support: multi-tab, templates, **swap layouts**, **floating panes**, and **stacked panes**.

2. **Two-phase geometry management**:
   - **Initial layout placement**: a recursive “split space” algorithm (percent/fixed/implicit) produces `PaneGeom` for each leaf pane.
   - **Interactive resize**: a *constraint solver* (cassowary) re-solves spans on resize events, then discretizes to integer terminal cells.

3. **Floating panes are a separate subsystem**:
   - Floating panes have their own store + z-order (`z_indices`) + pinning semantics.
   - Layout positioning is “find room” + apply user-requested coords, then clamp to viewport.

If FrankenTerm wants floating panes + robust resize in a mux-server context, the key “port” is: **represent pane geometry as a constraint system**, not as ad-hoc proportional math.

## Mental model / architecture diagram

```text
KDL (layout file) + config KDL
        │
        ▼
zellij-utils::input::layout
  - Layout / TiledPaneLayout / FloatingPaneLayout
  - parse + validation + helpful errors
        │
        ▼
zellij-server::tab::LayoutApplier
  - flatten layout -> leaf panes with PaneGeom (+ logical_position)
  - match panes by "exact run" then logical position
  - apply geom to TiledPanes + FloatingPanes
        │
        ├─────────────────────────┐
        ▼                         ▼
TiledPanes subsystem          FloatingPanes subsystem
  - grid + stacks               - overlay panes (z order)
  - cassowary resizer           - pinning + focus
```

## Layout system (KDL → Layout → PaneGeom)

### Where it lives

- Parser/types: `legacy_zellij/zellij-utils/src/input/layout.rs`
- Layout applier: `legacy_zellij/zellij-server/src/tab/layout_applier.rs`
- Swap layouts: `legacy_zellij/zellij-server/src/tab/swap_layouts.rs`

### The core types

In `zellij-utils`, the user-visible tab layout is:

- `Layout`:
  - `tabs: Vec<(Option<String>, TiledPaneLayout, Vec<FloatingPaneLayout>)>`
  - `template: Option<(TiledPaneLayout, Vec<FloatingPaneLayout>)>`
  - `swap_*_layouts`: sets of alternative layouts selected by `LayoutConstraint`

- `TiledPaneLayout` (tree):
  - `children_split_direction: SplitDirection`
  - `children: Vec<TiledPaneLayout>`
  - `split_size: Option<SplitSize>` where `SplitSize = Percent | Fixed`
  - `children_are_stacked`, `is_expanded_in_stack` (stacked panes)
  - `logical_position` is carried on the *geometry* (via `PaneGeom.logical_position`)

- `FloatingPaneLayout`:
  - position (`x`,`y`) and size (`width`,`height`) as `PercentOrFixed`
  - `pinned`, `logical_position`, `focus`, `borderless`, etc.

### Initial placement algorithm: `split_space`

Initial tiling is a recursive splitter:

- Entry point: `TiledPaneLayout::position_panes_in_space(..)`
- Core recursion: `split_space(..)` in `legacy_zellij/zellij-utils/src/input/layout.rs`

High-level behavior:

- Supports `Percent`, `Fixed`, and **implicit flexible** sizing (`None` → shares remaining percent).
- For **stacked panes**, it assigns `Fixed(1)` row to each non-expanded stack member and gives the remainder to the expanded pane (with a minimum-height safeguard).
- After converting percent/fixed to concrete sizes, it calls `adjust_geoms_for_rounding_errors(..)` to avoid gaps/overlaps due to float rounding.

This is primarily “layout-to-geometry” rather than interactive resizing.

## Swap layouts (one-key layout switching)

Swap layouts are modeled explicitly in the layout schema:

- `LayoutConstraint`:
  - `MaxPanes(n)`, `MinPanes(n)`, `ExactPanes(n)`, `NoConstraint`
- `SwapLayouts` selects the next layout whose constraints match the current pane counts.

Code:
- `legacy_zellij/zellij-utils/src/input/layout.rs` (`LayoutConstraint`, `SwapTiledLayout`, `SwapFloatingLayout`)
- `legacy_zellij/zellij-server/src/tab/swap_layouts.rs` (selection + “damaged/dirty” tracking)

Key idea to port:
- Swap-layout selection is a **function of state**, not hardcoded keybind logic. This makes it easy to add “swap to layout that fits N panes”.

## Floating panes (overlay model + pinning + z-order)

Floating panes are managed as a separate subsystem:

Code:
- `legacy_zellij/zellij-server/src/panes/floating_panes/mod.rs`
- `legacy_zellij/zellij-server/src/panes/floating_panes/floating_pane_grid.rs`

Notable data structures:

- `FloatingPanes.panes: BTreeMap<PaneId, Box<dyn Pane>>`
- `FloatingPanes.z_indices: Vec<PaneId>` (render order)
- `desired_pane_positions: HashMap<PaneId, PaneGeom>` (user-intent positions vs terminal-resize movement)

Layout positioning flow:

1. `find_room_for_new_pane()` proposes a starting `PaneGeom`
2. `PaneGeom.apply_floating_pane_position(..)` applies x/y/width/height from `FloatingPaneLayout`
3. Clamp to viewport bounds and propagate `pinned` and `logical_position`

Porting assessment:

- **Port**: z-index list, pinning semantics, “desired positions” vs “computed positions” split.
- **Reimagine**: FrankenTerm’s “floating” may map to “overlay panes in mux UI”, not necessarily PTY panes (WezTerm already supports some overlay/floating semantics at the GUI layer).

## Resize handling: constraint-based, not proportional

This is the critical differentiator from most tmux-like proportional resizing.

Code:
- `legacy_zellij/zellij-server/src/panes/tiled_panes/pane_resizer.rs`

Key facts:

- Uses the `cassowary` constraint solver (`Solver`, `Variable`, weighted `EQ` relations).
- Represents each pane as one or more **spans** (`Span`) depending on the resize direction.
- Adds constraints (minimum sizes, adjacency, and fixed-size panes) across spans.
- Solves to floating sizes, then **discretizes** to integer rows/cols while avoiding gaps and overlap.

This design supports:

- Minimum pane sizes
- Better behavior under repeated interactive resizing
- Predictable outcomes when multiple constraints compete

Porting assessment:

- **Strongly recommended** to port this conceptual approach into a FrankenTerm muxer as ft expands beyond the current WezTerm compatibility bridge topology.
- For FrankenTerm’s *current* architecture (swarm platform with a WezTerm compatibility bridge), the immediate value is:
  - a reference design for a future mux server, and
  - a cautionary tale: proportional resizing becomes unmaintainable as soon as floating/stacked panes exist.

## Concrete “steal this” checklist

1. **Constraint solver resize** (cassowary):
   - Prototype a `PaneResizer` equivalent in FrankenTerm for any internal pane grid representation.
   - Keep a clear separation: *layout placement* vs *interactive resizing*.

2. **Swap layouts selected by constraints**:
   - Make swap selection pure and testable: `fn pick_layout(state, candidates) -> Layout`.

3. **Floating panes z-order + pinning**:
   - If FrankenTerm models overlays, use `z_indices` + `pinned_on_top` as explicit policy.

4. **Logical positions in geometry**:
   - Zellij carries `logical_position` through layout application and uses it to match existing panes.
   - This is relevant for FrankenTerm snapshot/restore: stable “logical slot” beats “nth pane in list”.

## Code pointers (quick grep targets)

- Layout types / KDL integration:
  - `legacy_zellij/zellij-utils/src/input/layout.rs`
  - `legacy_zellij/zellij-utils/src/kdl/*`
- Layout application:
  - `legacy_zellij/zellij-server/src/tab/layout_applier.rs`
- Swap layouts:
  - `legacy_zellij/zellij-server/src/tab/swap_layouts.rs`
- Floating panes:
  - `legacy_zellij/zellij-server/src/panes/floating_panes/mod.rs`
  - `legacy_zellij/zellij-server/src/panes/floating_panes/floating_pane_grid.rs`
- Constraint-based resizing:
  - `legacy_zellij/zellij-server/src/panes/tiled_panes/pane_resizer.rs`

## Relationship to the earlier inventory doc

If you haven’t read it yet, start with:
- `evidence/zellij/INVENTORY.md` — crate map + server message bus + orchestration patterns

This doc assumes that context and drills into the muxer/layout surface specifically.
