# COMPREHENSIVE_ANALYSIS_OF_franken_mermaid

## Scope

- Project analyzed: `/Users/jemanuel/projects/frankenmermaid` (local checkout path corresponding to bead path `/dp/franken_mermaid`)
- Analysis workspace (this repo): `/Users/jemanuel/projects/frankenterm`
- Bead scope covered in this revision:
  - `ft-2vuw7.4.1` (`[franken_mermaid] Research and architecture/codeflow investigation`)
  - `ft-2vuw7.4.1.1` (`[franken_mermaid][R1] Repository topology and crate/module boundary inventory`)

## Purpose Summary

FrankenMermaid is a Rust 2024 workspace implementing a Mermaid-compatible diagram engine with a deterministic end-to-end pipeline:

1. Parse/detect diagram text into shared IR.
2. Compute layout (cycle handling, ranking, crossing minimization, coordinate assignment).
3. Render through multiple surfaces (SVG, terminal, canvas, WASM API).

The architecture is explicitly split into dedicated crates around that pipeline, with `fm-cli` as the default workspace entrypoint and `fm-wasm` as the web/JS integration boundary.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/README.md:30`
- `/Users/jemanuel/projects/frankenmermaid/README.md:312`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:12`

## Workspace Topology

Workspace shape (`resolver = "2"`):
- 8 member crates:
  - `fm-core`
  - `fm-parser`
  - `fm-layout`
  - `fm-render-svg`
  - `fm-render-canvas`
  - `fm-render-term`
  - `fm-wasm`
  - `fm-cli`
- Default member: `crates/fm-cli`.
- Edition/toolchain posture:
  - Workspace edition `2024`, rust-version `1.95`.
  - `rust-toolchain.toml` pins nightly and includes `wasm32-unknown-unknown` target.
- Release profile tuned for size globally (`opt-level = "z"`, `lto = true`, `panic = "abort"`, `strip = true`) with `fm-layout` overridden to `opt-level = 3` in both dev and release.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:1`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:12`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:17`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:19`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:43`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:50`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:53`
- `/Users/jemanuel/projects/frankenmermaid/rust-toolchain.toml:1`

## Crate Inventory and Boundary Roles

### Layer A: Shared Domain Kernel

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `fm-core` | Canonical diagram IR/types/errors/diagnostics | `lib` | Defines `MermaidDiagramIr`, `DiagramType`, node/edge/label primitives and shared domain schema. |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:120`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:327`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:357`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:847`

### Layer B: Parse and Normalization

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `fm-parser` | Type detection + Mermaid/DOT parsing + recovery into IR | `lib` | Owns detection confidence/method metadata, DOT routing, and parse evidence helpers. |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:3`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:74`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:353`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:378`

### Layer C: Layout Engine

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `fm-layout` | Layout algorithm selection + cycle strategy + traced layout pipeline | `lib` | Defines algorithm/cycle config and produces `DiagramLayout`/stats used by all renderers. |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:9`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:22`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:195`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:221`

### Layer D: Rendering Surfaces

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `fm-render-svg` | SVG document/element/theme generation from IR+layout | `lib` | Internal module split across a11y/attributes/defs/path/text/theme/transform. |
| `fm-render-term` | Terminal renderer + diff/minimap/ascii normalization | `lib` | Provides simple entrypoints and richer module-level rendering primitives. |
| `fm-render-canvas` | Canvas2D rendering backend via context abstraction | `lib` | Context trait + renderer/viewport/shapes modules for testable canvas rendering. |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:8`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:33`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:84`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:43`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:76`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:134`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:18`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:37`

### Layer E: Host Integration Boundaries

| Crate | Boundary Role | Target Types | Notes |
|---|---|---|---|
| `fm-wasm` | wasm-bindgen bridge for parse/detect/render APIs | `cdylib`, `rlib` | Runtime config merge layer + JS-exposed API wrappers. |
| `fm-cli` | End-user CLI orchestration | `bin` (`fm-cli`) | Commands: render/parse/detect/validate (+ optional watch/serve). |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/Cargo.toml:8`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:313`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:325`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:339`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:9`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:52`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:227`

## Intra-Workspace Dependency Boundary Map

Direct local crate dependency wiring:

- `fm-core`: no local dependencies.
- `fm-parser`: `fm-core`.
- `fm-layout`: `fm-core`.
- `fm-render-svg`: `fm-core`, `fm-layout`.
- `fm-render-term`: `fm-core`, `fm-layout`.
- `fm-render-canvas`: `fm-core`, `fm-layout`.
- `fm-wasm`: `fm-core`, `fm-layout`, `fm-parser`, `fm-render-canvas`, `fm-render-svg`.
- `fm-cli`: `fm-core`, `fm-layout`, `fm-parser`, `fm-render-svg`, `fm-render-term`.

Boundary implication:
- `fm-core` is the structural root.
- `fm-layout` and `fm-parser` form the compute spine.
- Renderers depend on layout+core but not on parser.
- `fm-cli` and `fm-wasm` are outer orchestration adapters, not algorithm owners.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:21`
- `cargo metadata --manifest-path /Users/jemanuel/projects/frankenmermaid/Cargo.toml --no-deps --format-version 1` (local command output, 2026-02-22)

## Module Boundary Inventory (R1)

### `fm-core`

- `font_metrics` internal module plus a broad shared type surface.
- Key structural entities:
  - `DiagramType`
  - `GraphDirection`
  - `IrNode` / `IrEdge`
  - `MermaidDiagramIr`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:3`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:120`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:189`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:327`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:357`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-core/src/lib.rs:847`

### `fm-parser`

- Submodules:
  - `dot_parser`
  - `ir_builder`
  - `mermaid_parser`
- Top-level entrypoints:
  - `detect_type_with_confidence`
  - `parse`
  - `parse_evidence_json`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:3`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:74`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:353`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:378`

### `fm-layout`

- Strategy/control primitives:
  - `LayoutAlgorithm`
  - `CycleStrategy`
  - `LayoutConfig`
- Core entrypoints:
  - `layout`
  - `layout_diagram`
  - `layout_diagram_traced`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:9`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:22`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:56`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:195`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:203`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:221`

### `fm-render-svg`

- Submodules:
  - `a11y`, `attributes`, `defs`, `document`, `element`, `path`, `text`, `theme`, `transform`
- Core API:
  - `SvgRenderConfig`
  - `render_svg`
  - `render_svg_with_config`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:8`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:33`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:84`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:90`

### `fm-render-term`

- Public modules:
  - `ascii`, `canvas`, `config`, `diff`, `glyphs`, `minimap`, `renderer`
- Core API:
  - `render_term`
  - `render_term_with_config`
  - `render_diff`
  - `render_minimap_simple`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:43`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:76`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:102`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:134`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:151`

### `fm-render-canvas`

- Submodules:
  - `context`, `renderer`, `shapes`, `viewport`
- Core API:
  - `render_to_canvas`
  - `render_canvas` (legacy compatibility helper)

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:18`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:37`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:49`

### `fm-wasm`

- JS/wasm exports and adapters include:
  - `init`
  - `renderSvg` (`render_svg_js`)
  - `detectType` (`detect_type_js`)
  - `parse` (`parse_js`)
- Also contains `Diagram` wrapper methods in wasm32 builds.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:324`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:338`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:347`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:353`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:602`

### `fm-cli`

- Subcommand surface centralized in `Command` enum.
- Main command handlers:
  - `cmd_render`
  - `cmd_parse`
  - `cmd_detect`
  - `cmd_validate`
- Optional capability flags in crate features:
  - `watch`, `serve`, `png`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:52`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:227`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:345`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:577`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:611`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:722`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:13`

## Runtime and Build Entrypoints

- Default workspace execution target: `fm-cli`.
- CLI binary entrypoint: `crates/fm-cli/src/main.rs`.
- WASM artifact entrypoint: `fm-wasm` (`cdylib`, `rlib`), built through `build-wasm.sh` (uses `wasm-pack` + `wasm-opt` + size budget enforcement).
- Integration test surface currently centered in `fm-cli/tests/integration_test.rs` for parse->layout->render flow.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:12`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:9`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:11`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/Cargo.toml:8`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:1`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:21`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:40`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/tests/integration_test.rs:1`

## R2 Build/Runtime/Dependency Map and Feature-Flag Matrix

### Build and Runtime Modes

| Mode | Primary Crates | Compile/Feature Toggles | Output/Runtime Boundary |
|---|---|---|---|
| Native CLI | `fm-cli` + parser/layout/render libs | `fm-cli` default features empty (base path) | Local process CLI (`fm-cli`) |
| CLI watch mode | `fm-cli` | `fm-cli/watch` -> enables `notify` | File-watch re-render loop in CLI process |
| CLI serve mode | `fm-cli` | `fm-cli/serve` -> enables `tiny_http` | Local HTTP preview server |
| CLI PNG mode | `fm-cli` + SVG raster deps | `fm-cli/png` -> enables `resvg`, `usvg` | Rasterized output path |
| WASM web mode | `fm-wasm` + parser/layout/render-canvas/render-svg | `wasm32-unknown-unknown` target + `build-wasm.sh` | Browser/JS runtime via wasm-bindgen |

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:13`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:16`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:17`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:18`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/Cargo.toml:8`
- `/Users/jemanuel/projects/frankenmermaid/rust-toolchain.toml:4`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:21`

### Intra-Workspace Dependency Graph (Build-Relevant)

Dependency chain by layer:
- `fm-core` <- `fm-parser`, `fm-layout`
- `fm-layout` + `fm-core` <- render crates (`fm-render-svg`, `fm-render-term`, `fm-render-canvas`)
- Parser/layout/render crates <- `fm-cli` and `fm-wasm`

Highest fan-in crates:
- `fm-core` (consumed by every other crate)
- `fm-layout` (consumed by all renderers and both host adapters)

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:21`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:22`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:23`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:24`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:25`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:26`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:27`
- `cargo metadata --manifest-path /Users/jemanuel/projects/frankenmermaid/Cargo.toml --no-deps --format-version 1` (local command output, 2026-02-22)

### Third-Party Dependency Domains

Workspace-level external dependencies cluster into these domains:
- Parsing and syntax:
  - `chumsky`, `json5`, `unicode-segmentation`
- CLI and diagnostics:
  - `clap`, `anyhow`, `tracing`, `tracing-subscriber`
- WASM/web bridge:
  - `wasm-bindgen`, `web-sys`, `js-sys`, `serde-wasm-bindgen`
- Serialization/error:
  - `serde`, `serde_json`, `thiserror`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:30`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:31`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:32`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:33`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:34`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:35`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:36`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:37`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:38`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:39`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:40`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:41`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/Cargo.toml:11`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/Cargo.toml:12`

### Compile-Time and Runtime Toggles that Matter for Integration

- Global profile policy is size-optimized by default, but `fm-layout` gets performance optimization (`opt-level = 3`) in both dev and release, indicating layout is treated as a hot path.
- WASM build path is externally tool-driven (`wasm-pack`, `wasm-opt`) and includes a gzip size budget gate (`500 KiB`) in `build-wasm.sh`.
- CLI behavior surface changes materially based on optional feature flags (`watch`, `serve`, `png`), which alters dependency closure and runtime behavior.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:43`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:50`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:53`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:9`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:12`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:45`
- `/Users/jemanuel/projects/frankenmermaid/build-wasm.sh:56`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:16`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:17`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:18`

## R3 Public Surface Inventory (APIs/CLI/MCP/Config/Events)

### CLI Public Surface (`fm-cli`)

Public commands (from `Command` enum):
- `render`
- `parse`
- `detect`
- `validate`
- optional: `watch` (feature-gated)
- optional: `serve` (feature-gated)

Public output/contracts exposed by CLI:
- output format selector: `svg`, `png`, `term`, `ascii`
- structured response types in command handlers:
  - `RenderResult`
  - `DetectResult`
  - `ValidateResult`
- additional serve/watch operational interfaces in `cmd_watch` and `cmd_serve`.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:52`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:163`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:176`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:193`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:203`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:233`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:266`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:274`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:850`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:933`

### Rust Library API Surfaces

Primary externally-consumable crate entrypoints:
- `fm-parser`:
  - `detect_type_with_confidence`
  - `parse`
  - `parse_evidence_json`
- `fm-layout`:
  - `layout`
  - `layout_diagram`
  - `layout_diagram_traced`
- `fm-render-svg`:
  - `render_svg`
  - `render_svg_with_config`
- `fm-render-term`:
  - `render_term`
  - `render_term_with_config`
  - `render_diff`
  - `render_minimap_simple`
- `fm-render-canvas`:
  - `render_to_canvas`
  - `render_canvas`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:74`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:353`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-parser/src/lib.rs:378`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:195`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:203`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-layout/src/lib.rs:221`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:84`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-svg/src/lib.rs:90`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:76`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:102`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:134`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-term/src/lib.rs:151`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:37`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-render-canvas/src/lib.rs:49`

### WASM/JS API Surface (`fm-wasm`)

Top-level exported functions:
- `init(config)`
- `renderSvg(input, config)`
- `detectType(input)`
- `parse(input)`

Stateful browser API object:
- `Diagram::new(canvas, config)`
- `Diagram::render(input, config)`
- `Diagram::setTheme(theme)`
- `Diagram::on(event, callback)`
- `Diagram::destroy()`

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:325`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:339`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:348`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:354`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:553`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:575`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:602`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:624`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:634`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:641`

### Configuration Surface Inventory

User-facing config channels:
- CLI/file-based TOML config documented in README:
  - `[core]`, `[parser]`, `[layout]`, `[render]`, `[svg]`, `[term]`
- Inline Mermaid init directives supported:
  - `%%{init: {...}}%%`
- WASM runtime config schema (JSON via JS interop):
  - root: `theme`, `svg`, `canvas`
  - SVG overrides: responsiveness/accessibility/typography/padding/theme toggles
  - Canvas overrides: typography/padding/style/autofit toggles

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/README.md:234`
- `/Users/jemanuel/projects/frankenmermaid/README.md:240`
- `/Users/jemanuel/projects/frankenmermaid/README.md:246`
- `/Users/jemanuel/projects/frankenmermaid/README.md:253`
- `/Users/jemanuel/projects/frankenmermaid/README.md:261`
- `/Users/jemanuel/projects/frankenmermaid/README.md:267`
- `/Users/jemanuel/projects/frankenmermaid/README.md:275`
- `/Users/jemanuel/projects/frankenmermaid/README.md:281`
- `/Users/jemanuel/projects/frankenmermaid/README.md:284`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:37`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:45`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:61`

### Events and Protocol Endpoints

Event model currently exposed for integration:
- browser-side DOM event subscription through `Diagram::on(event, callback)` against the canvas element.
- CLI watch mode also forms a file-change event loop (`cmd_watch`), but it is process-local.

Protocol/API endpoint inventory:
- no MCP server surface in this repo.
- no standalone network protocol API in the core crates; optional `serve` mode is a local HTTP preview utility, not a machine orchestration control plane.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:634`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:850`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:933`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/Cargo.toml:17`

## R4 Execution-Flow Tracing Across Core Workflows

### Workflow A: CLI `render` Path (Parse -> Layout -> Render -> Output)

Control flow in `cmd_render`:
1. Input acquisition: `load_input`.
2. Parse: `parse(&source)` from `fm-parser`.
3. Layout: `layout_diagram(&parsed.ir)` from `fm-layout`.
4. Render dispatch: `render_format(...)`.
5. Output write:
   - PNG branch -> `write_output_bytes`.
   - Other formats -> `write_output`.

Render-format sub-flow:
- `svg` -> `render_svg(ir)`.
- `png` -> `render_svg(ir)` then `svg_to_png(...)` (feature-gated).
- `term`/`ascii` -> `render_term_with_config(...)` with different glyph mode configs.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:345`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:357`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:373`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:405`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:438`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:448`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:456`

### Workflow B: CLI Analysis Paths (`parse`, `detect`, `validate`)

- `cmd_parse`:
  - `load_input` -> `parse`.
  - `full` mode emits full IR JSON.
  - summary mode emits `parse_evidence_json`.
- `cmd_detect`:
  - `load_input` -> `detect_type`.
  - confidence/method enrichment via `analyze_detection_confidence`.
- `cmd_validate`:
  - `load_input` -> `parse`.
  - maps parser warnings into validation warnings.
  - derives structural validation errors and emits result.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:577`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:586`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:595`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:611`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:620`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:722`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-cli/src/main.rs:735`

### Workflow C: WASM Stateless Exports

- `render(input)`:
  - `parse(input)` -> `layout(..., LayoutAlgorithm::Auto)` (stats side-effect) -> `render_svg_with_config`.
- `renderSvg(input, config)`:
  - parse/merge runtime overrides -> `parse(input)` -> `render_svg_with_config`.
- `detectType(input)`:
  - `detect_type_with_confidence`.
- `parse(input)`:
  - passthrough to parser output serialization.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:313`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:325`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:339`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:348`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:354`

### Workflow D: WASM Stateful `Diagram` Object

`Diagram::render` call chain:
1. Liveness guard (`ensure_alive`).
2. Config merge (SVG + canvas overrides).
3. Parse input -> SVG render (`render_svg_with_config`).
4. Canvas render (`render_to_canvas` via `WebCanvas2dContext`).
5. Package output as `DiagramRenderOutput`.

Supporting lifecycle methods:
- `new` initializes canvas context and merged runtime config.
- `setTheme` mutates SVG theme.
- `on` binds DOM event listener to canvas.
- `destroy` clears canvas and marks object inert.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:553`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:575`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:602`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:611`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:614`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:624`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:634`
- `/Users/jemanuel/projects/frankenmermaid/crates/fm-wasm/src/lib.rs:641`

## Repository Footprint Snapshot

`src/*.rs` footprint by crate (local measured snapshot):

- `fm-render-svg`: 10 files, 5193 LOC
- `fm-parser`: 4 files, 4961 LOC
- `fm-layout`: 1 file, 3682 LOC
- `fm-render-term`: 8 files, 3641 LOC
- `fm-render-canvas`: 5 files, 2069 LOC
- `fm-core`: 2 files, 1550 LOC
- `fm-cli`: 1 file, 1076 LOC
- `fm-wasm`: 1 file, 702 LOC

Interpretation:
- Current complexity concentration is strongest in rendering (`fm-render-svg`) and parsing/layout (`fm-parser`, `fm-layout`), while orchestration shells (`fm-cli`, `fm-wasm`) remain relatively thin.

## Legacy and Auxiliary Boundaries

- `legacy_mermaid_code/mermaid` exists as a compatibility/reference corpus boundary and is not part of workspace members.
- `evidence/contracts/` contains design contract documents that appear to support formal/architectural evidence packaging.

Evidence:
- `/Users/jemanuel/projects/frankenmermaid/README.md:470`
- `/Users/jemanuel/projects/frankenmermaid/Cargo.toml:2`
- `/Users/jemanuel/projects/frankenmermaid/evidence/contracts/e-graphs-crossing-minimization.md:1`

## R1 Completion Checklist

- [x] Workspace member topology mapped.
- [x] Crate-level ownership boundaries classified by layer.
- [x] Core module boundaries inventoried per crate.
- [x] Primary runtime/build entrypoints identified (CLI and WASM).
- [x] Local footprint snapshot captured to anchor downstream R2/R3 analysis.

## R2 Completion Checklist

- [x] Build/runtime mode matrix documented.
- [x] Intra-workspace dependency graph summarized with integration implications.
- [x] Feature-flag matrix and compile/runtime toggles captured.
- [x] Third-party dependency domains grouped for integration planning.

## R3 Completion Checklist

- [x] CLI command/public output surfaces inventoried.
- [x] Rust crate-level public APIs mapped for parser/layout/render layers.
- [x] WASM exports and stateful `Diagram` API surface inventoried.
- [x] Config surfaces mapped (TOML + inline Mermaid init + WASM runtime config schema).
- [x] Event/protocol endpoints characterized (DOM event hook + local serve/watch scope; no MCP surface).

## R4 Completion Checklist

- [x] CLI render flow traced end-to-end (input -> parse -> layout -> render -> output).
- [x] CLI parse/detect/validate flow paths traced.
- [x] WASM stateless export call chains traced.
- [x] WASM stateful `Diagram` lifecycle and render call chains traced.

## Notes for Next Beads

- `ft-2vuw7.4.1.5` should document IR/state persistence contracts and data lifecycle boundaries for integration planning.
- `ft-2vuw7.4.1.6` should evaluate reliability/performance/security/policy surfaces, especially around parser recovery, render memory behavior, and wasm/browser execution constraints.
