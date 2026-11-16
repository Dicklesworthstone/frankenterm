# Zellij Layout Engine Analysis (`wa-1xk9j`)

## Scope
This document analyzes Zellij layout internals relevant to FrankenTerm:
- pane topology model
- floating pane mechanics
- stacked pane mechanics
- KDL layout format and runtime application
- many-pane behavior and scaling risks

Source root: `legacy_zellij/` (local clone referenced by existing Zellij evidence docs).

## 1) Layout Model

### Core data types
- `Layout` carries tabs, templates, and swap layouts (`zellij-utils/src/input/layout.rs:698`).
- `TiledPaneLayout` is a recursive tree with split direction, optional split sizes, and stack flags (`zellij-utils/src/input/layout.rs:845`).
- `FloatingPaneLayout` carries optional x/y/width/height, pinning, focus, and logical position (`zellij-utils/src/input/layout.rs:786`).

### Placement algorithm (topology -> geometry)
- Initial pane geometry is computed by recursive `split_space(...)` (`zellij-utils/src/input/layout.rs:1833`).
- Split sizing supports fixed, percent, and implicit flexible sizing with rounding correction via `adjust_geoms_for_rounding_errors(...)` (`zellij-utils/src/input/layout.rs:2004`).
- Stacked branches are special-cased during split: non-expanded panes become one-line panes and one pane receives flexible remainder (`zellij-utils/src/input/layout.rs:1840`).

### Runtime application / reconciliation
- `LayoutApplier::flatten_layout(...)` calls `position_panes_in_space(...)`, then assigns monotonic `logical_position` values (`zellij-server/src/tab/layout_applier.rs:357`).
- Existing panes are matched in three passes:
1. exact match (run + logical position)
2. same logical position
3. best-effort leftover matching
- See `apply_tiled_panes_layout_to_existing_panes(...)` (`zellij-server/src/tab/layout_applier.rs:160`).

## 2) Floating Pane Mechanics

### Subsystem shape
- Floating panes are managed separately from tiled panes in `FloatingPanes` (`zellij-server/src/panes/floating_panes/mod.rs:36`).
- Key state:
  - `desired_pane_positions`: user-intent positions persisted across resize (`zellij-server/src/panes/floating_panes/mod.rs:47`)
  - `z_indices`: render/focus ordering (`zellij-server/src/panes/floating_panes/mod.rs:48`)

### Positioning and z-order
- `position_floating_pane_layout(...)` computes a default free slot, applies requested coordinates/sizing, then clamps to viewport (`zellij-server/src/panes/floating_panes/mod.rs:284`).
- Pinning is first-class (`FloatingPaneLayout.pinned`, `PaneGeom.is_pinned`) and pinned panes are sorted to top with `make_sure_pinned_panes_are_on_top(...)` (`zellij-server/src/panes/floating_panes/mod.rs:1329`).

### Slot finding heuristic
- `find_room_for_new_pane()` probes candidate geometries with bounded offsets and collision checks (`zellij-server/src/panes/floating_panes/floating_pane_grid.rs:796`).
- Hard stop guard `MAX_PANES = 100` prevents unbounded probe loops (`zellij-server/src/panes/floating_panes/floating_pane_grid.rs:15`).

## 3) Stacked Pane Mechanics

### Model
- Stacks are represented by shared `PaneGeom.stacked` IDs, not a separate node tree (`zellij-server/src/panes/tiled_panes/stacked_panes.rs:322`).
- `StackedPanes` operates over pane maps and can merge/split stacks and move focus within stacks (`zellij-server/src/panes/tiled_panes/stacked_panes.rs:13`).

### Resize behavior
- Stack minimum size is enforced by member count (`min_stack_height`) (`zellij-server/src/panes/tiled_panes/stacked_panes.rs:257`).
- Resize redistributes space by preserving one flexible pane and one-line siblings (`resize_panes_in_stack`) (`zellij-server/src/panes/tiled_panes/stacked_panes.rs:262`).
- The cassowary-based `PaneResizer` delegates stacked resize to `StackedPanes` when needed (`zellij-server/src/panes/tiled_panes/pane_resizer.rs:159`).

### Stack creation
- Vertical/horizontal stack composition paths explicitly recompute aggregate geometry and assign fresh stack IDs (`zellij-server/src/panes/tiled_panes/stacked_panes.rs:624`, `zellij-server/src/panes/tiled_panes/stacked_panes.rs:700`).

## 4) KDL Layout Format and Validation

### KDL parser guarantees
- `parse_percent_or_fixed(...)` validates strict numeric/percent forms with explicit errors (`zellij-utils/src/kdl/kdl_layout_parser.rs:248`).
- `parse_pane_node(...)` enforces stack semantics:
  - stacked panes must have children
  - stacked panes cannot be vertical
  - expanded panes must belong to a stack
- See `zellij-utils/src/kdl/kdl_layout_parser.rs:519`.
- `parse_floating_pane_node(...)` parses floating coordinates/sizes plus pin/focus metadata (`zellij-utils/src/kdl/kdl_layout_parser.rs:591`).

### Swap layouts / constraint-based selection
- Swap layouts are modeled as constraint maps (`ExactPanes`, `MinPanes`, `MaxPanes`) and selected against current pane counts (`zellij-server/src/tab/swap_layouts.rs:13`, `zellij-server/src/tab/swap_layouts.rs:181`).
- Example KDL with stacked + floating swaps is in `zellij-utils/assets/layouts/default.swap.kdl:53`.

## 5) Can layouts be saved/restored?
Yes.

- Session layout manifests serialize to KDL plus sidecar pane-content map (`serialize_session_layout`) (`zellij-utils/src/session_serialization.rs:41`).
- Manifest includes tiled/floating panes, geometry, run metadata, and optional pane contents (`zellij-utils/src/session_serialization.rs:15`).
- Resurrection path reads cached layout and reparses it via `Layout::from_kdl(...)` (`zellij-utils/src/sessions.rs:356`).

## 6) Pane Abstraction (terminal/plugin/command)

- Pane runtime abstraction is trait-based (`Pane` trait) with common geometry/render/scrollback methods (`zellij-server/src/tab/mod.rs:222`).
- Concrete pane runtimes include `TerminalPane` and `PluginPane`; both are instantiated through layout application paths (`zellij-server/src/tab/layout_applier.rs:555`, `zellij-server/src/tab/layout_applier.rs:613`).
- "Command panes" are represented as run instructions (`Run::Command`) attached to pane definitions rather than a separate pane type (`zellij-utils/src/input/layout.rs:258`).

## 7) Many-Pane Performance Characteristics

### What exists in source
- Zellij has explicit pane-limit control at CLI level (`max_panes`) with close-old-pane semantics (`zellij-utils/src/cli.rs:40`).
- Server is multi-threaded by subsystem (pty/screen/plugin/pty_writer/background_jobs), which helps isolate load (`zellij-server/src/lib.rs:326`, `zellij-server/src/lib.rs:1739`).
- Client fanout uses bounded buffer (depth 5000) and disconnect-on-overflow policy to avoid unbounded memory growth (`zellij-server/src/os_input_output.rs:359`).
- Scrollback memory is bounded by configurable per-pane `scroll_buffer_size` (default documented as 10000 lines) (`zellij-utils/assets/config/default.kdl:368`).

### Layout-engine scaling implications
- Tiled layout flatten/apply paths are mostly linear over pane count with additional matching passes.
- Floating placement is probe-and-collision based; cost grows with pane count and candidate probes (`zellij-server/src/panes/floating_panes/floating_pane_grid.rs:796`).
- Directional floating focus helpers sort candidate panes, introducing repeated `O(n log n)` operations for navigation (`zellij-server/src/panes/floating_panes/floating_pane_grid.rs:772`).
- Stacked resize uses a cassowary solve + discretization step, then stack-specific redistribution (`zellij-server/src/panes/tiled_panes/pane_resizer.rs:64`).

### Evidence gap
- No dedicated benchmark harness directories or criterion bench targets were found in the Zellij workspace clone (checked via filesystem/manifest scan).
- Existing coverage is primarily functional/snapshot tests (eg. `five_new_floating_panes`, stacked layout snapshot tests) rather than throughput benchmarks (`zellij-server/src/tab/unit/tab_integration_tests.rs:1551`, `zellij-server/src/tab/unit/layout_applier_tests.rs:2218`).

## 8) Recommendations for FrankenTerm

1. Keep Zellij’s explicit `logical_position` mapping in any layout restore/swap logic.
Why: it materially reduces remap drift during partial layout changes.

2. Port the "desired position" vs "current computed position" split for floating panes.
Why: it preserves user intent during viewport changes and avoids drift.

3. Adopt explicit z-order list + pin-to-top policy for overlays.
Why: deterministic focus/render ordering is essential once overlays exist.

4. Mirror constraint-checked KDL (or equivalent schema) validation for stack semantics.
Why: catches invalid layouts before runtime and shrinks failure surface.

5. Keep bounded fanout/backpressure behavior for pane-output subscribers.
Why: bounded queues prevent one slow consumer from destabilizing the session.

6. Add real scalability benches in FrankenTerm before adopting stack/floating features at swarm scale.
Why: Zellij has strong functional tests but limited explicit performance benchmarks in this clone.

## Direct FrankenTerm Cross-References
- Floating panes design track: `wa-2dd4s.2`
- Swap layouts design track: `wa-2dd4s.3`
- Session persistence and restore relevance: `wa-rsaf`
