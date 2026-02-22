# Comprehensive Analysis of franken_mermaid

> Integration research for FrankenTerm bead `ft-2vuw7.4.1`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Status: Complete

---

## R1: Repository Topology and Crate/Module Boundary Inventory

### Overview

franken_mermaid is a Rust-first Mermaid-compatible diagram rendering engine. It implements the full pipeline from text parsing to multi-backend rendering (SVG, Terminal, Canvas/WASM).

| Metric | Value |
|--------|-------|
| Total Rust LOC | 23,236 |
| Rust files | 33 |
| Workspace crates | 8 |
| Test functions | 299 |
| Rust edition | 2024 |
| Minimum toolchain | 1.95 |
| Unsafe code | `#![forbid(unsafe_code)]` in all 8 crates |

### Workspace Structure

```
franken_mermaid/
├── Cargo.toml                   # Workspace root, edition 2024
├── crates/
│   ├── fm-core/         (2 files)   # IR types, config, diagnostics
│   ├── fm-parser/       (4 files)   # Mermaid/DOT text → IR
│   ├── fm-layout/       (1 file)    # Layout algorithms (Sugiyama, Force, etc.)
│   ├── fm-render-svg/   (10 files)  # SVG rendering backend
│   ├── fm-render-term/  (8 files)   # Terminal/TUI rendering backend
│   ├── fm-render-canvas/ (5 files)  # Canvas2D/WASM rendering backend
│   ├── fm-wasm/         (1 file)    # wasm-bindgen JS/TS API
│   └── fm-cli/          (2 files)   # CLI reference implementation
```

### Crate Dependency Graph

```
fm-core (foundation)
  ├── fm-parser (depends on fm-core)
  ├── fm-layout (depends on fm-core)
  ├── fm-render-svg (depends on fm-core, fm-layout)
  ├── fm-render-term (depends on fm-core, fm-layout)
  ├── fm-render-canvas (depends on fm-core, fm-layout)
  ├── fm-wasm (depends on fm-core, fm-layout, fm-parser, fm-render-canvas, fm-render-svg)
  └── fm-cli (depends on fm-core, fm-layout, fm-parser, fm-render-svg, fm-render-term)
```

---

## R2: Build/Runtime/Dependency Map and Feature-Flag Matrix

### External Dependencies by Crate

| Crate | External Dependencies |
|-------|----------------------|
| **fm-core** | `serde`, `serde_json`, `thiserror` |
| **fm-parser** | `unicode-segmentation`, `json5`, `chumsky` |
| **fm-layout** | *(none — fm-core only)* |
| **fm-render-svg** | *(none — fm-core + fm-layout only)* |
| **fm-render-term** | *(none — fm-core + fm-layout only)* |
| **fm-render-canvas** | *(none — fm-core + fm-layout only)* |
| **fm-wasm** | `wasm-bindgen`, `web-sys`, `js-sys`, `serde-wasm-bindgen` |
| **fm-cli** | `clap`, `anyhow`, `tracing`, `tracing-subscriber`; optional: `notify`, `tiny_http`, `resvg`, `usvg` |

### Feature Flags (fm-cli)

| Feature | Purpose | Dependencies Added |
|---------|---------|-------------------|
| `watch` | File watcher mode (`fm-cli render --watch`) | `notify` |
| `serve` | HTTP server for SVG preview | `tiny_http` |
| `png` | SVG→PNG rasterization | `resvg`, `usvg` |

### Build Profile

- Release: `opt-level = "z"` (size optimization), LTO, single codegen unit, `panic = "abort"`, stripped
- fm-layout: `opt-level = 3` even in debug builds (consistent performance)
- WASM target: `wasm32-unknown-unknown` with wasm-pack

### Key Observation

The rendering crates (`fm-render-svg`, `fm-render-term`, `fm-render-canvas`) have **zero external dependencies** beyond the workspace crates. This makes them ideal for embedding — adding terminal diagram rendering to FrankenTerm would not increase the external dependency count.

---

## R3: Public Surface Inventory (APIs/CLI/MCP/Config/Events)

### Public API per Crate

#### fm-core — IR Types and Configuration

```rust
// Core IR
pub struct MermaidDiagramIr {
    pub nodes: Vec<IrNode>,
    pub edges: Vec<IrEdge>,
    pub ports: Vec<IrPort>,
    pub clusters: Vec<IrCluster>,
    pub labels: Vec<IrLabel>,
    pub constraints: Vec<IrConstraint>,
    pub meta: MermaidDiagramMeta,
    pub diagnostics: Vec<Diagnostic>,
}

// Key enums
pub enum DiagramType { Flowchart, Sequence, Class, State, ER, Gantt, Pie, ... } // 24+ variants
pub enum NodeShape { Rect, Rounded, Stadium, Diamond, Hexagon, Circle, ... }    // 21 shapes
pub enum ArrowType { Line, Arrow, ThickArrow, DottedArrow, Circle, Cross }
pub enum GraphDirection { TB, TD, LR, RL, BT }

// Configuration
pub struct MermaidConfig {
    pub enabled: bool,
    pub glyph_mode: GlyphMode,
    pub render_mode: MermaidRenderMode,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub route_budget: usize,
    pub layout_iteration_budget: usize,
    // ... 15+ configurable fields
}

// Diagnostics
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub category: DiagnosticCategory,
    pub message: String,
    pub span: Option<SourceSpan>,
    pub suggestion: Option<String>,
    pub expected: Option<String>,
    pub found: Option<String>,
    pub related: Vec<DiagnosticRelated>,
}
```

#### fm-parser — Parsing API

```rust
pub fn parse(input: &str) -> ParseResult;
pub fn detect_type_with_confidence(input: &str) -> DetectedType;
pub fn parse_mermaid_with_detection(input: &str) -> ParseResult;
pub fn looks_like_dot(input: &str) -> bool;
pub fn parse_dot(input: &str) -> ParseResult;
pub fn parse_evidence_json(input: &str) -> String; // JSON summary
```

#### fm-layout — Layout Algorithms

```rust
pub fn layout_diagram(ir: &MermaidDiagramIr) -> DiagramLayout;
pub fn layout_diagram_with_config(ir: &MermaidDiagramIr, config: LayoutConfig) -> DiagramLayout;
pub fn layout_diagram_traced(ir: &MermaidDiagramIr) -> (DiagramLayout, LayoutTrace);

pub struct DiagramLayout {
    pub nodes: Vec<LayoutNodeBox>,
    pub edges: Vec<LayoutEdgePath>,
    pub clusters: Vec<LayoutClusterBox>,
    pub bounds: LayoutRect,
    pub stats: LayoutStats,
}

pub enum LayoutAlgorithm { Auto, Sugiyama, Force, Tree, Radial, Gantt, Sankey, Grid, Timeline }
pub enum CycleStrategy { Greedy, DfsBack, MfasApprox, CycleAware }
```

#### fm-render-svg — SVG Rendering

```rust
pub fn render_svg(ir: &MermaidDiagramIr) -> String;
pub fn render_svg_with_config(ir: &MermaidDiagramIr, config: &SvgRenderConfig) -> String;

pub struct SvgRenderConfig {
    pub responsive: bool,
    pub accessible: bool,
    pub theme: ThemePreset,
    pub font_family: String,
    pub font_size: f64,
    // ...
}

pub enum ThemePreset { Default, Corporate, Neon, Monochrome, Pastel, HighContrast }
```

#### fm-render-term — Terminal Rendering

```rust
pub fn render_term(ir: &MermaidDiagramIr) -> TermRenderResult;
pub fn render_term_with_config(
    ir: &MermaidDiagramIr, config: &TermRenderConfig, cols: usize, rows: usize
) -> TermRenderResult;
pub fn diff_diagrams(old: &MermaidDiagramIr, new: &MermaidDiagramIr) -> DiagramDiff;
pub fn render_minimap(ir: &MermaidDiagramIr) -> String;

pub struct TermRenderConfig {
    pub tier: MermaidTier,           // Compact/Normal/Rich/Auto
    pub render_mode: MermaidRenderMode, // CellOnly/Braille/Block/HalfBlock/Auto
    pub glyph_mode: GlyphMode,      // Unicode/ASCII
    pub max_width: Option<usize>,
    pub max_height: Option<usize>,
    // ...
}
```

#### fm-render-canvas — Canvas2D Rendering

```rust
pub fn render_to_canvas(
    ir: &MermaidDiagramIr, context: &mut dyn Canvas2dContext, config: &CanvasRenderConfig
) -> CanvasRenderResult;

pub trait Canvas2dContext {
    fn begin_path(&mut self);
    fn move_to(&mut self, x: f64, y: f64);
    fn line_to(&mut self, x: f64, y: f64);
    fn stroke(&mut self);
    fn fill_text(&mut self, text: &str, x: f64, y: f64);
    // ...
}

pub struct MockCanvas2dContext; // For testing without web-sys
```

### CLI Interface (fm-cli)

```bash
fm-cli render input.mmd --format svg --output out.svg
fm-cli render input.mmd --format term --cols 120 --rows 40
fm-cli detect input.mmd --json
fm-cli validate input.mmd --strict
fm-cli render input.mmd --watch          # requires 'watch' feature
fm-cli render input.mmd --serve :8080    # requires 'serve' feature
```

---

## R4: Execution-Flow Tracing Across Core Workflows

### Pipeline: Text → Diagram

```
Input Text
    │
    ▼
┌─────────────────────────────────┐
│ 1. DETECTION (fm-parser)        │
│    detect_type_with_confidence() │
│    Strategies (in priority):     │
│    - ExactKeyword (1.0)          │
│    - FuzzyKeyword (0.7-0.85)     │
│    - ContentHeuristic (0.6-0.8)  │
│    - DotFormat (0.95)            │
│    - Fallback (0.3)              │
└──────────────┬──────────────────┘
               │ DetectedType { diagram_type, confidence }
               ▼
┌─────────────────────────────────┐
│ 2. PARSING (fm-parser → fm-core)│
│    parse_mermaid_with_detection()│
│    - Routes to type-specific     │
│      parser (flowchart, seq, etc)│
│    - chumsky parser combinators  │
│    - IrBuilder accumulates IR    │
│    - Best-effort recovery:       │
│      auto-creates placeholders   │
│      for dangling edges          │
└──────────────┬──────────────────┘
               │ ParseResult { ir: MermaidDiagramIr, diagnostics }
               ▼
┌─────────────────────────────────┐
│ 3. LAYOUT (fm-layout)           │
│    layout_diagram()              │
│    Algorithm auto-selection:     │
│    - Sugiyama: flowchart/seq/... │
│    - Force: ER, architecture     │
│    - Specialized: Gantt, Sankey  │
│                                  │
│    Sugiyama stages:              │
│    a) Cycle removal              │
│    b) Rank assignment            │
│    c) Crossing minimization      │
│    d) Coordinate assignment      │
└──────────────┬──────────────────┘
               │ DiagramLayout { nodes, edges, clusters, stats }
               ▼
┌─────────────────────────────────┐
│ 4. RENDERING (fm-render-*)      │
│    Backend selection:            │
│    - SVG: full HTML5 output      │
│    - Terminal: multi-tier TUI    │
│      * CellOnly (compact)        │
│      * Braille (2×4 px/cell)     │
│      * Block (2×2), HalfBlock    │
│    - Canvas: web-sys trait       │
│    - WASM: JS/TS via bindgen     │
└──────────────┬──────────────────┘
               │ String (SVG) | TermRenderResult | CanvasRenderResult
               ▼
            Output
```

### Cycle Handling (fm-layout)

The Sugiyama layout must operate on DAGs. franken_mermaid handles cycles via:

1. **CycleStrategy::Greedy** — Heuristic edge reversal for simple graphs
2. **CycleStrategy::DfsBack** — DFS-based back-edge detection
3. **CycleStrategy::MfasApprox** — Minimum Feedback Arc Set approximation
4. **CycleStrategy::CycleAware** — Collapse cycle clusters for visual grouping

Reversed edges are marked with `reversed: true` in `LayoutEdgePath` so renderers can adjust arrowheads.

### Crossing Minimization

Uses barycenter heuristic + transpose refinement + sifting. `LayoutStats` tracks `crossing_count_before_refinement` and final count for quality assessment.

### Terminal Rendering Pipeline (fm-render-term)

```
IR + TermRenderConfig
    │
    ├── Auto-resolve tier/mode from terminal dimensions
    │
    ├── CellOnly path: character grid with box-drawing glyphs
    │   └── Direct string construction
    │
    └── Sub-cell Canvas path:
        ├── Allocate Canvas (cell_width × cell_height × multiplier)
        ├── Bresenham line drawing for edges
        ├── Rectangle/circle primitives for nodes
        ├── Label truncation/wrapping
        └── Canvas → string (Braille/Block/HalfBlock encoding)
```

Key detail: Canvas uses a **generation counter** for O(1) clear operations instead of zeroing the pixel array.

---

## R5: Data/State/Persistence Contract Analysis

### Core Data Model

All types derive `Serialize, Deserialize` via serde:

| Type | Fields | Purpose |
|------|--------|---------|
| `MermaidDiagramIr` | nodes, edges, ports, clusters, labels, constraints, meta, diagnostics | Central IR graph |
| `IrNode` | id, label (IrLabelId), shape, classes, span, members | Diagram node |
| `IrEdge` | from/to (IrEndpoint), arrow, label, span | Connection |
| `IrPort` | node (IrNodeId), name, side_hint | Subgraph port |
| `IrCluster` | id, title, members, span | Subgraph grouping |
| `IrLabel` | text, span | Shared text label |
| `IrConstraint` | SameRank, MinLength, Pin, OrderInRank | Layout directives |
| `DiagramLayout` | nodes, edges, clusters, bounds, stats | Computed positions |

### State Management

- **Immutable pipeline**: IR is built once during parsing, consumed by layout, consumed by rendering
- **No internal mutation** after construction (functional pipeline)
- **LayoutTrace** captures stage snapshots for debugging (opt-in via `layout_diagram_traced()`)
- **MermaidGuardReport** tracks complexity metrics and degradation plans

### Persistence

- **No native persistence** — the library is a pure transform pipeline
- JSON serialization available for all types (via serde)
- `parse_evidence_json()` produces summary JSON for logging/auditing
- Consumers (CLI, WASM) handle I/O

### Font Metrics

Deterministic cross-platform text measurement using precomputed character width tables:
- `FontPreset` enum for proportional/monospace detection
- `CharWidthClass` for unicode-aware width computation
- Ensures layout stability across platforms (no system font queries)

---

## R6: Reliability/Performance/Security/Policy Surface Analysis

### Error Handling

**Best-effort recovery philosophy** — the parser never panics on malformed input:

1. Auto-creates placeholder nodes for dangling edges
2. Unresolved endpoints tracked but don't block IR construction
3. Rich `Diagnostic` system with severity (Hint/Info/Warning/Error), category, span, suggestion
4. `parse()` always returns `ParseResult` — consumers check `ir.has_errors()` or examine diagnostics
5. Layout operations cannot fail (best-effort positioning)
6. Builder-style diagnostic API: `Diagnostic::error(msg).with_category(...).with_span(...).with_suggestion(...)`

### Safety

- **`#![forbid(unsafe_code)]`** in all 8 crates — zero unsafe anywhere
- All platform APIs abstracted behind traits (`Canvas2dContext`)
- WASM bindings use safe wasm-bindgen
- All strings owned (no unsafe slices)
- Unicode-aware via `unicode-segmentation`

### Performance

| Component | Complexity | Notes |
|-----------|-----------|-------|
| Detection | O(n) starts_with → O(n*m) Levenshtein | Multi-strategy, early exit |
| Parsing | O(n) with chumsky | Backtracking on choice operators |
| Rank assignment | O(V+E) | Longest path in DAG |
| Crossing minimization | O(V²) | Barycenter + transpose + sifting |
| Coordinate assignment | O(V + E log E) | Edge segment processing |
| Force-directed | O(V² × iterations) | Early exit on convergence |
| SVG render | O(V+E) | Single-pass string building |
| Terminal canvas clear | O(1) | Generation counter technique |

fm-layout uses `opt-level = 3` even in debug builds for consistent performance.

### Input Validation (DoS Prevention)

```rust
pub struct MermaidConfig {
    pub max_nodes: usize,           // Prevent graph explosion
    pub max_edges: usize,           // Prevent edge explosion
    pub max_label_chars: usize,     // Prevent label bombing
    pub max_label_lines: usize,     // Prevent multiline abuse
    pub route_budget: usize,        // Prevent routing iteration blowup
    pub layout_iteration_budget: usize, // Prevent layout iteration blowup
}
```

### Graceful Degradation

`MermaidGuardReport` + `MermaidDegradationPlan` enable progressive quality reduction:
- `hide_labels` — skip label rendering for dense graphs
- `collapse_clusters` — merge subgraphs
- `simplify_routing` — use straight lines instead of routed edges
- `reduce_decoration` — remove shadows, rounded corners
- `force_glyph_mode(ASCII)` — fallback if unicode rendering fails

### Determinism

- FNV-1a hash for deterministic initial positions in force-directed layout
- No randomness in tie-breaking (BTreeMap for ordered iteration)
- Layout output stable across runs with same input/config

---

## R7: Integration Seam Discovery and Upstream Tweak Opportunities

### FrankenTerm Integration Candidates

#### 1. Workflow/State Visualization (PRIMARY)

FrankenTerm could render live diagrams of:
- Agent workflow state machines (state diagrams)
- Pane topology and split layouts (flowcharts)
- Event bus message flows (sequence diagrams)
- Dependency graphs from beads (directed graphs)

**Integration path**: `fm-parser` + `fm-layout` + `fm-render-term`

```rust
// In FrankenTerm's TUI rendering
use fm_parser::parse;
use fm_render_term::{render_term_with_config, TermRenderConfig};

fn render_workflow_diagram(mermaid_text: &str, cols: usize, rows: usize) -> String {
    let result = parse(mermaid_text);
    let config = TermRenderConfig::compact(); // or .rich() for braille
    let rendered = render_term_with_config(&result.ir, &config, cols, rows);
    rendered.output
}
```

#### 2. Diff Visualization (HIGH VALUE)

`fm-render-term::diff_diagrams()` can show before/after state changes:
- Pane topology changes (panes added/removed/resized)
- Workflow step progression
- Agent state transitions

#### 3. Minimap Rendering

`fm-render-term::render_minimap()` provides compact overview rendering suitable for dashboard panels.

#### 4. IR Construction Without Parsing

FrankenTerm can bypass the parser and construct IR directly from its internal data structures:

```rust
let mut ir = MermaidDiagramIr::empty(DiagramType::Flowchart);
ir.meta.direction = GraphDirection::LR;

// Add panes as nodes
for pane in panes {
    ir.nodes.push(IrNode {
        id: format!("pane_{}", pane.id),
        label: ir.add_label(&pane.title),
        shape: NodeShape::Rounded,
        ..Default::default()
    });
}

// Add connections as edges
for conn in connections {
    ir.edges.push(IrEdge {
        from: IrEndpoint::Node(conn.from_idx),
        to: IrEndpoint::Node(conn.to_idx),
        arrow: ArrowType::Arrow,
        ..Default::default()
    });
}
```

#### 5. Robot Mode Diagram Output

FrankenTerm's `ft robot` commands could output diagrams:
- `ft robot diagram panes` — render pane topology as terminal diagram
- `ft robot diagram workflow <name>` — render workflow state
- `ft robot diagram events --last=50` — render recent event flow

### Dependency Cost for Integration

**Minimal integration** (terminal rendering only):
- Add: `fm-core`, `fm-layout`, `fm-render-term`
- External deps added: `serde` (already in frankenterm), `serde_json` (already), `thiserror` (already)
- **Net new external deps: ZERO**

**Full integration** (parsing + terminal + SVG):
- Add: `fm-core`, `fm-parser`, `fm-layout`, `fm-render-term`, `fm-render-svg`
- External deps added: `unicode-segmentation`, `json5`, `chumsky`
- **Net new external deps: 3**

### Upstream Tweak Proposals

1. **`pub fn MermaidDiagramIr::empty(diagram_type: DiagramType) -> Self`** — Constructor for programmatic IR building (may already exist, needs verification)

2. **Builder API for IrNode/IrEdge** — Fluent API for constructing IR without knowing all struct fields:
   ```rust
   ir.add_node("pane_1").label("Shell").shape(NodeShape::Rounded);
   ir.add_edge("pane_1", "pane_2").arrow(ArrowType::Arrow).label("sends to");
   ```

3. **`TermRenderResult` additions**:
   - `cell_map: Vec<Vec<Option<NodeId>>>` — map terminal cells back to nodes for click detection
   - `dirty_region()` — compute minimal redraw region for incremental updates

4. **Streaming/incremental layout** — For live-updating diagrams, support incremental node addition without full relayout

5. **Custom NodeShape registration** — Allow FrankenTerm to define custom shapes (e.g., terminal pane shape, agent shape)

---

## R8: Research Evidence Pack and Completeness Checklist

### Evidence Summary

| Research Area | Status | Key Finding |
|---------------|--------|-------------|
| R1: Repository topology | Complete | 8-crate workspace, 23,236 LOC, 33 files |
| R2: Build/deps | Complete | Zero-dep rendering crates; 3 features (watch/serve/png) |
| R3: Public API surface | Complete | 6 library crates with clean public APIs |
| R4: Execution flows | Complete | Linear pipeline: detect→parse→layout→render |
| R5: Data/persistence | Complete | Immutable IR pipeline, full serde support, no native persistence |
| R6: Reliability/security | Complete | forbid(unsafe_code) everywhere, best-effort recovery, DoS guards |
| R7: Integration seams | Complete | Terminal rendering with zero new deps; IR builder for direct construction |

### Strategic Fit Assessment

| Criterion | Score | Rationale |
|-----------|-------|-----------|
| **Architectural alignment** | 9/10 | Same Rust 2024, forbid(unsafe_code), serde-based types |
| **Dependency cost** | 10/10 | Zero new external deps for terminal rendering |
| **API cleanliness** | 8/10 | Clean trait-based design; IR could use builder API |
| **Performance fit** | 8/10 | Synchronous, deterministic, O(1) canvas clear |
| **Feature relevance** | 7/10 | Terminal rendering + diff + minimap directly useful |
| **Maintenance burden** | 8/10 | Small codebase (23K LOC), focused scope |
| **Overall** | **8.3/10** | Strong candidate for visualization layer |

### Top Integration Candidates (Ranked)

1. **fm-render-term** (terminal diagram rendering) — Direct value for TUI dashboards
2. **fm-core** (IR types) — Foundation for programmatic diagram construction
3. **fm-layout** (graph layout algorithms) — Reusable for pane topology layout
4. **fm-render-svg** (SVG output) — For MCP tool responses and export
5. **fm-parser** (text parsing) — For accepting Mermaid text input from agents

### Diagram Types Most Relevant to FrankenTerm

| Diagram Type | FrankenTerm Use Case |
|-------------|---------------------|
| **Flowchart** | Pane topology, agent workflows, event flows |
| **State** | Agent state machines, pane lifecycle |
| **Sequence** | IPC message flows, robot mode command chains |
| **Class** | Module dependency visualization |
| **ER** | Data model relationships |
| **Gantt** | Timeline visualization of pane activity |

---

*Analysis complete. Ready for integration plan creation (ft-2vuw7.4.3).*
