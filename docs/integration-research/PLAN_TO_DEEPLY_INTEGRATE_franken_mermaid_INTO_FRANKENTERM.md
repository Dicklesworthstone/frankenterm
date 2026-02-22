# Plan to Deeply Integrate franken_mermaid into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.4.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_franken_mermaid.md (ft-2vuw7.4.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Terminal diagram rendering for TUI dashboards**: Embed `fm-render-term` to render pane topologies, workflow states, and event flows as terminal diagrams in FrankenTerm's observation panels
2. **Programmatic IR construction**: Use `fm-core` IR types to build diagrams from FrankenTerm's internal data structures (PaneInfo, workflow steps, event bus messages) without parsing Mermaid text
3. **Diff visualization**: Use `fm-render-term::diff_diagrams()` to show before/after topology changes when panes are added, removed, or reconfigured
4. **Robot mode diagram commands**: Add `ft robot diagram` subcommands that output visual representations of system state
5. **SVG export for MCP tools**: Use `fm-render-svg` to generate diagram images for MCP tool responses and documentation export

### Constraints

- **Zero new external dependencies for terminal rendering**: fm-render-term depends only on fm-core + fm-layout, which require `serde`, `serde_json`, `thiserror` — all already in FrankenTerm
- **No unsafe code**: franken_mermaid uses `#![forbid(unsafe_code)]` in all 8 crates; this aligns with frankenterm-core's policy
- **Synchronous core**: franken_mermaid is synchronous; wrapping in async boundaries for FrankenTerm's async runtime is acceptable
- **Edition compatibility**: Both projects use Rust 2024 edition
- **No feature regression**: Existing FrankenTerm APIs must continue working unchanged

### Non-Goals

- **Replacing existing terminal output**: Diagrams complement text output, not replace it
- **WASM rendering**: `fm-wasm` and `fm-render-canvas` are not needed for FrankenTerm's server-side use case
- **Full Mermaid parsing in production**: IR is constructed programmatically from FrankenTerm data; text parsing is optional (for user-provided diagrams)
- **Interactive diagram editing**: Diagrams are read-only visual output
- **fm-cli integration**: FrankenTerm has its own CLI; fm-cli is a reference implementation only

---

## P2: Evaluate Integration Patterns

### Option A: Direct Embedding (Chosen)

Embed `fm-core`, `fm-layout`, and `fm-render-term` as workspace path dependencies in frankenterm-core. Optionally add `fm-render-svg` and `fm-parser`.

**Pros**: Zero external dep increase for terminal rendering, type-safe, compile-time checked, sub-millisecond rendering
**Cons**: Increases frankenterm-core compile surface by ~15K LOC (fm-core + fm-layout + fm-render-term)

### Option B: Subprocess Rendering

Shell out to `fm-cli render --format term` for diagram generation.

**Pros**: Process isolation, independent updates
**Cons**: Subprocess overhead (~50ms per render), deployment complexity, can't construct IR programmatically
**Rejected**: FrankenTerm needs sub-millisecond rendering for live dashboards; subprocess latency is unacceptable

### Option C: Feature-Gated Adapter

Add franken_mermaid integration behind a `diagram-viz` feature flag.

**Pros**: Optional compilation, minimal impact on default build
**Cons**: Feature flag testing matrix complexity
**Considered for Phase 1**: Start with feature gate, remove once proven stable

### Decision: Option A with initial feature gate (Option C wrapper)

Phase 1 uses `#[cfg(feature = "diagram-viz")]` to gate all franken_mermaid integration. Phase 2 removes the gate after regression suite confirms no issues.

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── diagram_bridge.rs          # NEW: Bridge from FrankenTerm types to fm-core IR
│   ├── diagram_render.rs          # NEW: Rendering helpers using fm-render-term/svg
│   └── ...existing modules...
├── Cargo.toml                     # Add path deps: fm-core, fm-layout, fm-render-term, ...
```

### Module Responsibilities

#### `diagram_bridge.rs` (FrankenTerm → IR translation)

Converts FrankenTerm's internal data structures into franken_mermaid IR:

- `pane_topology_to_ir(panes: &[PaneInfo]) -> MermaidDiagramIr` — Renders pane tree as flowchart
- `workflow_state_to_ir(workflow: &WorkflowState) -> MermaidDiagramIr` — Renders workflow as state diagram
- `event_flow_to_ir(events: &[Event]) -> MermaidDiagramIr` — Renders event sequence as sequence diagram
- `dependency_graph_to_ir(deps: &[(String, String)]) -> MermaidDiagramIr` — Generic directed graph

Key design: Each function constructs IR directly using `MermaidDiagramIr::empty()` + node/edge insertion, bypassing the parser entirely. This avoids the `fm-parser` dependency for the core use case.

#### `diagram_render.rs` (rendering orchestration)

Wraps fm-render-term and fm-render-svg with FrankenTerm-specific defaults:

- `render_diagram_term(ir: &MermaidDiagramIr, cols: usize, rows: usize) -> String` — Terminal output with auto tier/mode
- `render_diagram_svg(ir: &MermaidDiagramIr) -> String` — SVG output with FrankenTerm theme
- `render_diagram_diff(old: &MermaidDiagramIr, new: &MermaidDiagramIr) -> String` — Diff visualization
- `render_minimap(ir: &MermaidDiagramIr) -> String` — Compact overview

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
diagram-viz = ["fm-core", "fm-layout", "fm-render-term"]
diagram-svg = ["diagram-viz", "fm-render-svg"]
diagram-parse = ["diagram-viz", "fm-parser"]

[dependencies]
fm-core = { path = "../../franken_mermaid/crates/fm-core", optional = true }
fm-layout = { path = "../../franken_mermaid/crates/fm-layout", optional = true }
fm-render-term = { path = "../../franken_mermaid/crates/fm-render-term", optional = true }
fm-render-svg = { path = "../../franken_mermaid/crates/fm-render-svg", optional = true }
fm-parser = { path = "../../franken_mermaid/crates/fm-parser", optional = true }
```

**Alternative**: Use `[patch]` section if franken_mermaid is published, or symlink via `/dp/franken_mermaid`.

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: DiagramBridge

```rust
#[cfg(feature = "diagram-viz")]
pub mod diagram_bridge {
    use fm_core::{MermaidDiagramIr, DiagramType, GraphDirection, NodeShape, ArrowType, IrEndpoint};

    /// Render pane topology as a flowchart diagram IR
    pub fn pane_topology_to_ir(panes: &[PaneInfo], direction: GraphDirection) -> MermaidDiagramIr {
        let mut ir = MermaidDiagramIr::empty(DiagramType::Flowchart);
        ir.meta.direction = direction;
        // Add panes as nodes with shape based on pane type
        // Add parent-child edges based on split topology
        ir
    }

    /// Render workflow state machine as state diagram IR
    pub fn workflow_state_to_ir(
        states: &[WorkflowStateNode],
        transitions: &[WorkflowTransition],
        current_state: Option<&str>,
    ) -> MermaidDiagramIr {
        let mut ir = MermaidDiagramIr::empty(DiagramType::State);
        // Add states as nodes (current_state highlighted)
        // Add transitions as edges with labels
        ir
    }

    /// Render event sequence as sequence diagram IR
    pub fn event_flow_to_ir(
        participants: &[String],
        messages: &[(usize, usize, String)], // (from_idx, to_idx, label)
    ) -> MermaidDiagramIr {
        let mut ir = MermaidDiagramIr::empty(DiagramType::Sequence);
        // Add participants as nodes
        // Add messages as edges
        ir
    }

    /// Render generic directed graph
    pub fn directed_graph_to_ir(
        nodes: &[(String, String)],           // (id, label)
        edges: &[(String, String, Option<String>)], // (from_id, to_id, label)
    ) -> MermaidDiagramIr;
}
```

### Public API Contract: DiagramRender

```rust
#[cfg(feature = "diagram-viz")]
pub mod diagram_render {
    use fm_core::MermaidDiagramIr;
    use fm_render_term::{TermRenderConfig, TermRenderResult, MermaidTier};

    pub struct DiagramRenderConfig {
        pub tier: MermaidTier,           // Compact/Normal/Rich/Auto
        pub max_label_chars: usize,      // Default: 20
        pub show_clusters: bool,         // Default: true
    }

    impl Default for DiagramRenderConfig {
        fn default() -> Self {
            Self {
                tier: MermaidTier::Auto,
                max_label_chars: 20,
                show_clusters: true,
            }
        }
    }

    /// Render diagram to terminal string
    pub fn render_term(
        ir: &MermaidDiagramIr,
        config: &DiagramRenderConfig,
        cols: usize,
        rows: usize,
    ) -> TermRenderResult;

    /// Render diagram diff showing topology changes
    pub fn render_diff(
        old: &MermaidDiagramIr,
        new: &MermaidDiagramIr,
        cols: usize,
        rows: usize,
    ) -> String;

    /// Render compact minimap overview
    pub fn render_minimap(ir: &MermaidDiagramIr) -> String;

    /// Render to SVG (requires diagram-svg feature)
    #[cfg(feature = "diagram-svg")]
    pub fn render_svg(ir: &MermaidDiagramIr) -> String;
}
```

### Crate Extraction Roadmap

**Phase 1**: Keep franken_mermaid crates as external path dependencies via `/dp/franken_mermaid`

**Phase 2**: If franken_mermaid is published to crates.io, switch to version dependencies:
```toml
fm-core = "0.1"
fm-layout = "0.1"
fm-render-term = "0.1"
```

**Phase 3**: If tight coupling develops, extract `ft-diagram` as a separate frankenterm crate that wraps fm-* with FrankenTerm-specific logic.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — this is a new visualization capability, not a replacement:
- Diagrams are generated on-demand from current state
- No stored diagram data to migrate
- No existing visualization APIs to deprecate

### State Synchronization

- **Input**: FrankenTerm's PaneInfo, workflow state, events (existing data)
- **Output**: Terminal strings, SVG strings (ephemeral — not stored)
- **No bidirectional sync** — diagrams are pure functions of current state

### Compatibility Posture

- **Additive only**: No existing APIs change; diagram_bridge/diagram_render add new capabilities
- **Backward compatible**: If `diagram-viz` feature is disabled, behavior is identical to current
- **Forward compatible**: IR construction functions accept version-tagged input types
- **Graceful degradation**: If terminal is too small, auto-select compact tier or skip rendering

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `diagram_bridge.rs` tests (target: 30+)

- `test_empty_pane_list_produces_empty_ir` — edge case
- `test_single_pane_produces_single_node` — simplest case
- `test_pane_topology_node_count` — correct node count
- `test_pane_topology_edge_count` — correct edge count
- `test_pane_topology_direction_lr` — horizontal layout
- `test_pane_topology_direction_tb` — vertical layout
- `test_pane_shape_by_type` — shell vs agent vs idle pane shapes
- `test_workflow_state_nodes` — state diagram node creation
- `test_workflow_transition_edges` — state transitions as edges
- `test_workflow_current_state_highlight` — active state annotation
- `test_event_flow_participants` — sequence diagram participants
- `test_event_flow_messages` — sequence diagram messages
- `test_directed_graph_basic` — generic graph construction
- `test_directed_graph_labels` — edge labels preserved
- `test_large_pane_topology` — 100+ panes
- `test_disconnected_panes` — panes with no relationships
- `test_cyclic_dependencies` — cycles handled gracefully
- Additional edge cases and boundary conditions to reach 30+

#### `diagram_render.rs` tests (target: 30+)

- `test_render_term_basic` — produces non-empty output
- `test_render_term_compact` — compact tier output
- `test_render_term_rich` — rich tier with braille
- `test_render_term_auto_tier` — auto-selects based on dimensions
- `test_render_term_small_terminal` — graceful degradation
- `test_render_term_large_diagram` — handles many nodes
- `test_render_diff_no_changes` — identical diagrams
- `test_render_diff_added_node` — node addition highlighted
- `test_render_diff_removed_node` — node removal highlighted
- `test_render_minimap_basic` — compact overview output
- `test_render_svg_basic` — produces valid SVG
- `test_render_svg_theme` — FrankenTerm theme applied
- `test_render_config_defaults` — default config correctness
- `test_render_config_custom` — custom config applied
- `test_render_unicode_labels` — non-ASCII text handling
- `test_render_empty_diagram` — empty IR renders safely
- Additional rendering edge cases to reach 30+

### Integration Tests

- `test_pane_topology_render_roundtrip` — PaneInfo → IR → terminal string
- `test_workflow_state_render_roundtrip` — workflow → IR → terminal string
- `test_feature_gate_disabled` — verify no regression when feature disabled
- `test_diagram_in_robot_mode` — robot mode produces diagram output
- `test_svg_export_via_mcp` — MCP tool returns valid SVG

### Property-Based Tests (proptest)

- `proptest_pane_topology_node_count` — node count equals pane count
- `proptest_pane_topology_edge_count` — edge count ≤ node_count - 1 (tree) or more (with cycles)
- `proptest_render_term_nonempty` — any valid IR produces non-empty output
- `proptest_render_term_fits_dimensions` — output fits requested cols/rows
- `proptest_diff_symmetric` — diff(a,b) and diff(b,a) both succeed
- `proptest_minimap_smaller` — minimap dimensions ≤ full render dimensions

### Logging Requirements

```rust
tracing::debug!(
    node_count = %ir.nodes.len(),
    edge_count = %ir.edges.len(),
    diagram_type = ?ir.meta.diagram_type,
    tier = ?config.tier,
    cols = %cols,
    rows = %rows,
    "diagram_render.render_term"
);
```

Fields: `diagram_type`, `node_count`, `edge_count`, `tier`, `render_mode`, `cols`, `rows`, `render_time_us`

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Feature-Gated Integration (Week 1-2)**
1. Add fm-core, fm-layout, fm-render-term as optional path dependencies behind `diagram-viz` feature
2. Implement `diagram_bridge.rs` with pane topology and workflow state translation
3. Implement `diagram_render.rs` with terminal rendering wrapper
4. Write unit tests (30+ per module)
5. Gate: `cargo check --workspace --all-targets` passes with and without feature

**Phase 2: Wiring Into Runtime (Week 3-4)**
1. Add `ft robot diagram panes` command using diagram_bridge + diagram_render
2. Add `ft robot diagram workflow <name>` command
3. Wire into observation TUI panel (optional diagram view)
4. Integration tests
5. Gate: All existing tests pass, diagram commands produce valid output

**Phase 3: SVG Export and MCP (Week 5)**
1. Enable `diagram-svg` feature
2. Add MCP tool for diagram generation (`ft_diagram_panes`, `ft_diagram_workflow`)
3. Add `ft robot diagram --format=svg` option
4. Gate: SVG output validates against SVG spec

**Phase 4: Default Enable (Week 6)**
1. Remove feature gate (always-on for terminal rendering)
2. Run full regression suite
3. Performance benchmarking
4. Gate: No performance regression >5%

### Rollback Plan

- **Phase 1 rollback**: Remove feature flag, revert Cargo.toml changes (single commit)
- **Phase 2 rollback**: Feature flag disable, diagram commands return "not available"
- **Phase 3 rollback**: Disable `diagram-svg` feature
- **Phase 4 rollback**: Re-introduce feature flag

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Compile time increase | Medium | Low | fm-render-term + deps are ~15K LOC total |
| Layout performance on large topologies | Low | Medium | MermaidConfig limits (max_nodes, route_budget) |
| Terminal rendering artifacts | Medium | Low | Multiple tier/mode fallbacks (Rich→Normal→Compact) |
| Diagram too large for terminal | Medium | Low | Auto-tier selection, minimap fallback |
| API instability in fm-* | Low | Medium | Pin to specific commit/version |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace` (all existing tests pass)
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. New module tests: 60+ tests across 2 modules
5. Integration tests: 5+ cross-module tests
6. Property-based tests: 6+ proptest scenarios
7. No memory or performance regression in benchmark suite

---

## P8: Summary and Action Items

### Chosen Architecture

**Direct embedding** of 3 franken_mermaid crates (`fm-core`, `fm-layout`, `fm-render-term`) as optional path dependencies, initially behind a `diagram-viz` feature gate. Optional `fm-render-svg` behind `diagram-svg` feature. Optional `fm-parser` behind `diagram-parse` feature.

### Two New Modules

1. **`diagram_bridge.rs`**: Translates FrankenTerm types (PaneInfo, WorkflowState, Events) into franken_mermaid IR without text parsing
2. **`diagram_render.rs`**: Wraps fm-render-term/svg with FrankenTerm-specific defaults, diff visualization, and minimap

### Implementation Order

1. Add Cargo.toml dependencies (feature-gated)
2. Implement `diagram_bridge.rs` with 30+ unit tests
3. Implement `diagram_render.rs` with 30+ unit tests
4. Add `ft robot diagram` commands
5. Integration and property-based tests
6. SVG export and MCP tools
7. Performance benchmarking
8. Remove feature gate (default-on)

### Key Advantages Over franken_redis Integration

| Aspect | franken_mermaid | franken_redis |
|--------|----------------|---------------|
| New external deps | 0 (terminal) / 3 (with parser) | 0 |
| Primary value | Visual output (diagrams) | Data management (session state) |
| Complexity | Lower (pure transform) | Higher (stateful store) |
| User-visible impact | Immediate (visual dashboards) | Internal (state management) |

### Upstream Tweak Proposals (for franken_mermaid)

1. **Builder API for IR construction**: Fluent `ir.add_node("id").label("text").shape(Rounded)` API
2. **Cell-to-node mapping**: `TermRenderResult.cell_map` for click/hover detection in TUI
3. **Dirty region tracking**: `render_incremental()` for efficient live-updating diagrams
4. **Custom shape registration**: Allow consumers to define domain-specific node shapes
5. **Theme extension API**: Allow FrankenTerm to define a custom `ThemePreset` with branded colors

### Beads Created/Updated

- `ft-2vuw7.4.1` (CLOSED): Research complete
- `ft-2vuw7.4.2` (CLOSED): Analysis document complete
- `ft-2vuw7.4.3` (THIS DOCUMENT): Integration plan complete
- Next: `ft-2vuw7.4.3.*` sub-beads → implementation beads

---

*Plan complete. Ready for review and implementation bead creation.*
