# Comprehensive Analysis of FrankenMermaid

> Bead: ft-2vuw7.4.1 / ft-2vuw7.4.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

FrankenMermaid (`/dp/franken_mermaid`) is a modular diagram rendering engine implementing 24+ Mermaid diagram types in ~23K LOC across 8 Rust crates. It provides parsing (Chumsky-based with recovery), deterministic layout (Sugiyama + force-directed), and multi-target rendering (SVG, terminal, Canvas2D, WASM). All crates enforce `#![forbid(unsafe_code)]` with only 14 external dependencies.

**Key characteristics:**
- 24+ diagram types (flowchart, sequence, class, state, ER, Gantt, pie, git graph, etc.)
- Best-effort parsing with intent inference and diagnostic collection
- Deterministic layout with stable tie-breaking (same input = identical output)
- Multi-target rendering: SVG (zero-dep), terminal (braille/block/ASCII), Canvas2D (trait-based), WASM
- 10 theme presets with CSS custom properties and accessibility (ARIA)

**Integration relevance to FrankenTerm:** High. FrankenTUI's `ftui-extras` already contains mermaid modules (`mermaid.rs`, `mermaid_layout.rs`, `mermaid_render.rs`) that map 1:1 to FrankenMermaid crates. Direct replacement path: `fm-core` → `fm-parser` → `fm-layout` → `fm-render-term`.

---

## 2. Repository Topology

### 2.1 Workspace Structure (8 crates)

```
/dp/franken_mermaid/   (v0.1.0, edition 2024, MSRV 1.95, MIT)
├── fm-core/          (1,550 LOC)  — IR types, config, diagnostics, errors
├── fm-parser/        (4,961 LOC)  — Chumsky Mermaid/DOT parser with recovery
├── fm-layout/        (3,682 LOC)  — Sugiyama/force/tree/specialized layout
├── fm-render-svg/    (4,939 LOC)  — Zero-dep SVG generation + themes + a11y
├── fm-render-term/   (3,641 LOC)  — Terminal: braille, block, ASCII, minimap, diff
├── fm-render-canvas/ (2,069 LOC)  — Canvas2D via trait abstraction + mock
├── fm-wasm/          (702 LOC)    — JavaScript/WASM bindings
└── fm-cli/           (1,076 LOC)  — CLI: render, parse, detect, validate, watch, serve
```

### 2.2 Pipeline

```
Input text → fm-parser (detect + parse) → fm-core::MermaidDiagramIr
    → fm-layout (Sugiyama/Force/Tree) → DiagramLayout
    → fm-render-{svg,term,canvas} → output
    → fm-cli / fm-wasm (surfaces)
```

### 2.3 External Dependencies (14 total)

| Dep | Purpose |
|-----|---------|
| chumsky 0.10 | Parser combinator |
| clap 4.5 | CLI |
| serde / serde_json | Serialization |
| thiserror 2.0 | Error derives |
| tracing | Logging |
| wasm-bindgen / web-sys / js-sys | WASM |
| unicode-segmentation | Text handling |
| json5 | Lenient JSON |
| notify (optional) | File watching |
| tiny_http (optional) | HTTP server |
| resvg/usvg (optional) | PNG rasterization |

---

## 3. Key Capabilities

### 3.1 Parsing
- **24+ diagram types** with fuzzy keyword matching (Levenshtein ≤ 2)
- **Content heuristics** (pattern detection for ER, Sequence, Class, State, Flowchart)
- **DOT bridge** for Graphviz interop
- **Confidence scoring**: Exact (1.0), Fuzzy (0.7-0.85), Heuristic (0.6-0.8), Fallback (0.3)
- **Recovery**: Malformed input → IR + diagnostics (never hard failure)

### 3.2 Layout
- **Algorithms**: Sugiyama (hierarchical), Force (Fruchterman-Reingold), Tree, Radial, Timeline, Gantt, Sankey, Grid
- **Cycle handling**: Greedy, DFS back-edge, MFAS approximation, cycle-aware
- **Crossing minimization**: Barycenter, transpose, sifting
- **Deterministic**: Stable tie-breaking, BTreeMap ordering
- **Stats**: crossing counts, reversed edges, cycle metrics

### 3.3 Rendering
- **SVG**: Zero-dep document builder, 10 themes, ARIA accessibility, responsive sizing
- **Terminal**: Braille (2x4), Block (2x2), HalfBlock (1x2), CellOnly; minimap; diagram diff
- **Canvas**: Trait-based (`Canvas2dContext`), MockCanvas for testing, viewport management
- **WASM**: `@frankenmermaid/core` npm package, lazy config, Diagram class

### 3.4 CLI
```bash
fm-cli render [INPUT] --format {svg|png|term|ascii} --theme {theme}
fm-cli parse [INPUT] --full --pretty
fm-cli detect [INPUT] --json
fm-cli validate [INPUT] --json --strict
fm-cli watch {path} --format {fmt}
fm-cli serve --port 8080
```

---

## 4. Integration Opportunities with FrankenTerm

### 4.1 Direct Replacement Path

FrankenTUI's `ftui-extras` already has mermaid modules that map to FrankenMermaid crates:

| ftui-extras module | FrankenMermaid crate | Action |
|-------------------|---------------------|--------|
| `mermaid.rs` | `fm-parser` | Replace parser |
| `diagram.rs` | `fm-core` | Replace IR types |
| `mermaid_layout.rs` | `fm-layout` | Replace layout engine |
| `mermaid_render.rs` | `fm-render-svg` + `fm-render-term` | Replace renderers |
| `mermaid_diff.rs` | `fm-render-term::diff` | Replace differ |
| `mermaid_minimap.rs` | `fm-render-term::minimap` | Replace minimap |
| `dot_parser.rs` | `fm-parser` (DOT bridge) | Replace DOT parser |
| `canvas.rs` | `fm-render-canvas` | Replace Canvas trait |

### 4.2 Extraction Candidates

| Component | Source | Effort | Risk |
|-----------|--------|--------|------|
| IR types (DiagramType, NodeShape, etc.) | fm-core | Low | Low |
| Parser + detection | fm-parser | Low | Low |
| Layout engine | fm-layout | Low | Medium |
| Terminal renderer | fm-render-term | Low | Low |
| SVG renderer | fm-render-svg | Low | Low |

### 4.3 Migration Path

1. **Phase 1**: Depend on `fm-core` for IR types
2. **Phase 2**: Replace parser with `fm-parser::parse()`
3. **Phase 3**: Replace layout with `fm-layout::layout_diagram()`
4. **Phase 4**: Replace terminal renderer with `fm-render-term`
5. **Phase 5** (optional): SVG renderer, WASM bindings

### 4.4 Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Early version (v0.1.0) | Medium | Comprehensive tests, determinism checks |
| API evolution | Medium | Pin version, design carefully |
| Performance regression | Low | fm-layout has opt-level=3 override |
| Dependency bloat | Low | 14 crates total, most small |

---

## 5. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Modular diagram rendering engine (24+ Mermaid types) |
| **Size** | ~23K LOC, 8 crates |
| **Architecture** | Pipeline: parse → IR → layout → render (stateless, deterministic) |
| **Safety** | `#![forbid(unsafe_code)]`, bounded parsers, no code execution |
| **Integration Value** | High — direct 1:1 replacement for ftui-extras mermaid modules |
| **Top Extraction** | fm-core, fm-parser, fm-layout, fm-render-term |
| **Risk** | Low — clean APIs, comprehensive tests, minimal deps |
