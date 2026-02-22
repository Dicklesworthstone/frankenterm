# Comprehensive Analysis of FrankenTUI

> Bead: ft-2vuw7.3.1 / ft-2vuw7.3.2
> Author: DarkMill
> Date: 2026-02-22

## 1. Executive Summary

FrankenTUI (`/dp/frankentui`) is a deterministic, scrollback-native terminal UI kernel implemented in ~620K LOC across 19 Rust crates. It provides an Elm/Bubbletea-style runtime with 50+ widgets, constraint-based layout, cache-aligned 16-byte cell rendering, and inline-first architecture that preserves terminal scrollback. All core crates enforce `#![forbid(unsafe_code)]`.

**Key characteristics:**
- **Inline-first**: Default to preserving scrollback with stable UI region (not full-screen alt-screen)
- **Deterministic rendering**: Same state + size + theme = identical frame
- **Flicker-free by construction**: All output diffed and buffered through single writer
- **Elm architecture**: `Model::update()` + `Model::view()` with `Cmd` side-effects
- **Feature-rich**: 50+ widgets, markdown, charts, clipboard, syntax highlighting, visual FX, games

**Integration relevance to FrankenTerm:** High. FrankenTUI provides the rendering kernel, event model, and layout primitives that FrankenTerm can adopt directly. Key extraction candidates include canonical `Event` type, `TerminalCapabilities` detection, text width calculation, `Cell` layout, and the `Backend` trait abstraction.

---

## 2. Repository Topology

### 2.1 Workspace Structure (19 crates)

```
/dp/frankentui/
├── Cargo.toml              (workspace, edition 2024, resolver 2)
├── rust-toolchain.toml      (nightly)
└── crates/
    ├── ftui/                (957 LOC)    — public facade + prelude
    ├── ftui-core/           (33K LOC)    — terminal lifecycle, events, input parsing
    ├── ftui-render/         (41K LOC)    — render kernel: cells, buffers, diffs, presenter
    ├── ftui-backend/        (542 LOC)    — backend trait abstractions
    ├── ftui-layout/         (20K LOC)    — flex/grid constraint solver, pane management
    ├── ftui-text/           (27K LOC)    — text spans, rope editor, width caching
    ├── ftui-style/          (8K LOC)     — colors, themes, CSS-like cascading
    ├── ftui-runtime/        (66K LOC)    — Elm-style runtime, subscriptions, telemetry
    ├── ftui-widgets/        (78K LOC)    — 50+ widget library
    ├── ftui-extras/         (136K LOC)   — feature-gated add-ons (markdown, charts, FX)
    ├── ftui-harness/        (18K LOC)    — test fixtures, snapshots, trace recording
    ├── ftui-pty/            (9K LOC)     — PTY-backed test utilities
    ├── ftui-tty/            (3K LOC)     — native Unix terminal backend
    ├── ftui-web/            (7K LOC)     — WASM backend, host-driven rendering
    ├── ftui-i18n/           (1K LOC)     — localization
    ├── ftui-simd/           (17 LOC)     — reserved for SIMD optimizations
    ├── ftui-demo-showcase/  (113K LOC)   — demo app with 50+ screens
    ├── ftui-showcase-wasm/  (3K LOC)     — WASM showcase runner
    └── doctor_frankentui/   (7K LOC)     — CLI diagnostics tool
```

### 2.2 Crate Dependency Graph

```
ftui-backend (trait layer, leaf)
    │
ftui-core (leaf: events, input, capabilities)
    │
ftui-style (leaf: colors, themes)
    │
ftui-text (text primitives, rope, width cache)
    │
ftui-render (cell, buffer, diff, presenter)
    │
ftui-layout (flex, grid, pane tree)
    │
ftui-runtime (Elm loop, subscriptions, telemetry)
    │   ├── ftui-backend, ftui-core, ftui-layout
    │   ├── ftui-render, ftui-style, ftui-text, ftui-i18n
    │   └── optional: ftui-tty, OpenTelemetry
    │
ftui-widgets (50+ widgets)
    │   └── ftui-core, ftui-layout, ftui-render, ftui-style, ftui-text
    │
ftui-extras (feature-gated add-ons)
    │   └── ftui-widgets + conditional: image, markdown, wgpu, vte
    │
ftui (public facade: re-exports all above)
```

### 2.3 External Dependencies

**Key direct dependencies (384 transitive total):**

| Dependency | Version | Purpose |
|------------|---------|---------|
| `ahash` | 0.8 | Fast hash maps |
| `bitflags` | 2.11 | Bitfield macros |
| `smallvec` | 1.15 | Small vector optimization |
| `bumpalo` | 3.20 | Arena allocator |
| `ropey` | 1.6 | Rope data structure |
| `unicode-*` | various | Width, segmentation, bidi |
| `crossterm` | 0.29 | Legacy terminal backend (optional) |
| `nix` | 0.31 | Unix syscalls (ftui-tty only) |
| `clap` | 4.5 | CLI parsing (doctor tool) |
| `serde` / `serde_json` | 1.0 | Serialization (feature-gated) |
| `wgpu` | 28.0 | GPU rendering (optional) |
| `pulldown-cmark` | 0.13 | Markdown parsing (optional) |
| `vte` | 0.15 | Terminal emulation (optional) |
| `proptest` | 1.7 | Property testing (dev) |

### 2.4 Build Configuration

- **Edition:** 2024 (nightly required)
- **Release profile:** `opt-level = "z"`, LTO, single codegen unit, strip symbols
- **No build.rs**, no custom proc macros
- **100+ feature flags** across crates

---

## 3. Architecture & Core Data Flow

### 3.1 Rendering Pipeline

```
Input (Terminal/WebSocket)
    ↓ ftui-core: InputParser → Event (canonical)
    ↓ ftui-runtime: Program → Model::update(msg) → Cmd
    ↓ ftui-runtime: Model::view(&self, frame)
    ↓ ftui-render: Buffer (16-byte cells, cache-aligned)
    ↓ ftui-render: BufferDiff (row-major, dirty tracking)
    ↓ ftui-render: Presenter (stateful ANSI emitter)
    ↓ ftui-runtime: TerminalWriter (single-buffered write)
    ↓ Terminal output / scrollback
```

### 3.2 Key Architectural Patterns

**Elm Architecture (Model-View-Update):**
```rust
pub trait Model: Send + 'static {
    type Message: Send + 'static;
    fn update(&mut self, msg: Self::Message) -> Cmd<Self::Message>;
    fn view(&self, frame: &mut Frame);
    fn subscriptions(&self) -> Vec<Subscription<Self::Message>> { vec![] }
}
```

**Cache-Aligned Cell (16 bytes):**
- Content (4 bytes) + FG (4 bytes, PackedRgba) + BG (4 bytes) + Attrs (4 bytes)
- 80x24 terminal = 1920 cells = 30KB (fits in L1 cache)

**Inline Mode Strategies:**
- `ScrollRegion`: DECSTBM for modern terminals
- `OverlayRedraw`: Save cursor → clear → redraw → restore
- `Hybrid`: Auto-selects based on terminal capabilities

**One-Writer Rule:**
- `TerminalWriter` owns stdout exclusively
- Prevents interleaving between log output and UI rendering
- `LogSink` provides safe alternative for application logging

### 3.3 Module Responsibilities

| Module | Responsibility | Key Types |
|--------|---------------|-----------|
| `ftui-core` | Terminal lifecycle, events, input parsing, capability detection | `Event`, `TerminalSession`, `TerminalCapabilities`, `InputParser`, `Cx` |
| `ftui-render` | Cell grid, diffing, ANSI output | `Buffer`, `Cell`, `BufferDiff`, `Frame`, `Presenter` |
| `ftui-backend` | Platform abstraction traits | `BackendEventSource`, `BackendPresenter`, `BackendClock` |
| `ftui-layout` | Constraint-based layout, pane topology | `Flex`, `Grid`, `Constraint`, `PaneTree`, `Rect` |
| `ftui-text` | Styled text, rope, width measurement | `Text`, `Span`, `Segment`, `Rope`, `WidthCache` |
| `ftui-style` | Colors, themes, cascading styles | `Style`, `Color`, `Theme`, `ColorProfile` |
| `ftui-runtime` | Event loop, command dispatch, state persistence | `Program`, `Model`, `Cmd`, `TerminalWriter` |
| `ftui-widgets` | 50+ UI components | `Widget` trait, `List`, `Table`, `TextInput`, `Paragraph` |
| `ftui-extras` | Feature-gated add-ons | Markdown, charts, clipboard, visual FX, terminal widget |

---

## 4. Public API Surface

### 4.1 Core Types (ftui prelude)

```rust
// Events
pub enum Event { Key(KeyEvent), Mouse(MouseEvent), Resize{..}, Paste(..), Focus(..) }
pub struct KeyEvent { code: KeyCode, modifiers: Modifiers, kind: KeyEventKind }
pub enum KeyCode { Char(char), F(u8), Enter, Esc, Tab, Up, Down, Left, Right, ... }

// Rendering
pub struct Buffer { /* 2D cell grid with scissor/opacity stacks */ }
pub struct Cell { content: Grapheme, fg: PackedRgba, bg: PackedRgba, attrs: CellAttrs }
pub struct Frame { buffer: &mut Buffer, cursor_position: Option<(u16,u16)>, ... }

// Layout
pub enum Constraint { Fixed(u16), Percentage(f32), Min(u16), Max(u16), Fill, FitContent, ... }
pub struct Flex { /* constraint-based 1D solver */ }
pub struct Grid { /* 2D layout solver */ }
pub struct Rect { x: u16, y: u16, width: u16, height: u16 }

// Style
pub struct Style { fg: Option<Color>, bg: Option<Color>, flags: StyleFlags }
pub enum Color { Reset, Ansi(u8), Rgb(u8,u8,u8), ... }
pub struct Theme { primary, background, text, accent, success, warning, error, ... }

// Runtime
pub trait Model { type Message; fn update(&mut self, msg) -> Cmd; fn view(&self, frame); }
pub struct Program { /* Elm-style event loop */ }
pub enum ScreenMode { Fullscreen, Inline { ui_height: u16 } }

// Widgets
pub trait Widget { fn render(&self, area: Rect, frame: &mut Frame); }
pub trait StatefulWidget<S> { fn render(&self, area: Rect, frame: &mut Frame, state: &mut S); }
```

### 4.2 Widget Library (50+ widgets)

**Containers:** Block, Panel, Modal, Group, Padding
**Text:** Paragraph, Rule, Tiles
**Selection:** List, Table, Tree, Tabs, Paginator
**Input:** TextInput, TextArea, CommandPalette, FilePicker
**Progress:** ProgressBar, Gauge, Spinner, Timer, Stopwatch
**Visualization:** Sparkline, BarChart, LineChart, ScatterChart
**Interactive:** Scrollbar, Draggable, DropTarget, DragHandle
**Feedback:** Toast, NotificationQueue, Badge, Help
**Advanced:** Inspector, DebugOverlay, JsonView, LogViewer

### 4.3 Feature Flags (Major)

| Feature | Crate | Effect |
|---------|-------|--------|
| `render-thread` | ftui-runtime | Dedicated render/output thread |
| `state-persistence` | ftui-runtime | JSON file storage for widget state |
| `telemetry` | ftui-runtime | OpenTelemetry OTLP export |
| `native-backend` | ftui-runtime | Native TTY backend (Unix) |
| `crossterm-compat` | ftui-runtime | Legacy Crossterm event source |
| `canvas` | ftui-extras | Drawing primitives |
| `charts` | ftui-extras | Data visualization |
| `clipboard` | ftui-extras | Clipboard access |
| `markdown` | ftui-extras | Markdown rendering |
| `syntax` | ftui-extras | Syntax highlighting |
| `visual-fx` | ftui-extras | Visual effects (metaballs, plasma) |
| `fx-gpu` | ftui-extras | GPU acceleration (wgpu) |
| `terminal-widget` | ftui-extras | Terminal emulation |

### 4.4 CLI Tool (doctor_frankentui)

```bash
doctor_frankentui capture <profile>    # Profile-driven VHS capture
doctor_frankentui seed-demo            # Seed MCP demo data
doctor_frankentui suite <config>       # Multi-profile suite
doctor_frankentui report <suite-dir>   # Generate HTML/JSON reports
doctor_frankentui doctor               # Validate environment
doctor_frankentui list-profiles        # List available profiles
```

---

## 5. State & Persistence

### 5.1 Persisted Data

| Data | Format | Backend | Purpose |
|------|--------|---------|---------|
| Widget state | JSON | FileStorage / MemoryStorage | Cross-session widget persistence |
| Terminal capabilities | OnceLock | In-memory cache | Fast capability lookup |
| Render traces | JSONL | File (EvidenceSink) | Frame-by-frame diagnostics |
| Telemetry spans | OTLP | OpenTelemetry exporter | Distributed tracing |
| Grapheme widths | LRU cache | In-memory | Unicode width memoization |

### 5.2 State Management

- **Single-threaded update loop**: Model updates on same thread as event loop
- **Frame-per-render**: Frame created fresh each cycle (no persistent frame state)
- **Optional render thread**: Dedicated output thread via `render-thread` feature
- **StateRegistry**: `Arc<RwLock<HashMap>>` with pluggable storage backend

---

## 6. Reliability, Performance & Security

### 6.1 Error Handling
- Graceful degradation: capability detection failure → safe defaults
- Terminal cleanup: RAII `TerminalSession::Drop` always restores terminal state
- Panic hook: best-effort cleanup on unwinding
- Input bounds: CSI/DCS length bounded, bracketed paste size bounded

### 6.2 Performance
- **Cell layout**: 16 bytes, cache-aligned (80x24 = 30KB, fits L1)
- **Diff target**: <1ms for 120x40 UI
- **Input parse**: <100us per event
- **Row-major scan**: Cache-friendly diffing
- **Grapheme fast path**: ASCII width = 1 (no lookup)

### 6.3 Security
- `#![forbid(unsafe_code)]` in all core crates
- No FFI in default build
- Bounded input parsers (no unbounded allocation)
- Single-writer output (no interleaving)
- JSON state with schema versioning (v32)

---

## 7. Integration Opportunities with FrankenTerm

### 7.1 High-Value Extraction Candidates

| Component | Source | Lines | FrankenTerm Use Case | Effort |
|-----------|--------|-------|---------------------|--------|
| **Event type** | ftui-core | ~500 | Canonical input model replacing custom enum | Medium |
| **TerminalCapabilities** | ftui-core | ~800 | Shared terminal detection (truecolor, mux, sync output) | Medium |
| **text_width** | ftui-core | ~200 | Grapheme width calculation (shared foundation) | Low |
| **Backend trait** | ftui-backend | 542 | Platform abstraction (native/web/custom) | Low |
| **Cell + Buffer** | ftui-render | ~2K | Standard rendering primitives | Medium |
| **BufferDiff** | ftui-render | ~1K | Incremental output optimization | High |
| **Rect + Constraint** | ftui-layout | ~500 | Layout geometry primitives | Low |
| **InlineMode** | ftui-runtime | ~1K | Scrollback-native UI rendering | High |

### 7.2 Existing Integration Points

1. **ftui-pty**: WsPtyBridge for WebSocket-to-PTY bridging, already integrates with `frankenterm-core` via `flow_control` module
2. **ftui-web**: WASM backend with `DeterministicClock` and `WebEventSource` for FrankenTerm web frontend
3. **Telemetry**: Both projects emit JSONL evidence; standardizing on shared sink is straightforward

### 7.3 Integration Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| Nightly toolchain requirement | Low | Both projects already on nightly/2024 edition |
| 384 transitive deps | Medium | Feature-gate heavy deps (wgpu, image, etc.) |
| Synchronous core | Low | FrankenTerm can wrap in async boundaries |
| Breaking API changes | Medium | Adopt types incrementally, maintain compatibility layer |

### 7.4 Recommended Integration Path

**Phase 1: Shared Primitives (1-2 days)**
- Extract `text_width`, `Rect`, `Constraint` into shared foundation
- Adopt `Backend` trait as platform abstraction

**Phase 2: Event & Capabilities (3-5 days)**
- Adopt canonical `Event` type from ftui-core
- Share `TerminalCapabilities` detection logic

**Phase 3: Rendering Integration (1-2 weeks)**
- Adopt `Cell` + `Buffer` as rendering standard
- Integrate `BufferDiff` for incremental output
- Wire `InlineMode` for scrollback-native UI

**Phase 4: Widget Integration (2-3 weeks)**
- Use ftui-widgets for FrankenTerm operator dashboard
- Integrate state persistence for cross-session widget state
- Add ftui-runtime's `Program` for interactive UI components

---

## 8. Coupling Hotspots

### 8.1 High Coupling
- **ftui-runtime ↔ ftui-render**: Runtime directly manages Buffer lifecycle and diff strategy
- **ftui-widgets ↔ ftui-layout**: Widgets depend heavily on Constraint and Flex APIs
- **ftui-extras ↔ ftui-widgets**: Extras extend Widget trait with heavy feature-gated deps

### 8.2 Low Coupling (Good Extraction Targets)
- **ftui-backend**: Pure trait definitions, zero implementation deps
- **ftui-core::text_width**: Pure functions, no state
- **ftui-style**: Colors and themes independent of rendering
- **ftui-i18n**: Standalone localization catalog

---

## 9. Summary

| Dimension | Finding |
|-----------|---------|
| **Purpose** | Deterministic, scrollback-native terminal UI kernel |
| **Size** | ~620K LOC, 19 crates, 50+ widgets |
| **Architecture** | Elm-style Model-View-Update with single-writer rendering |
| **Safety** | `#![forbid(unsafe_code)]`, bounded parsers, RAII cleanup |
| **Performance** | Cache-aligned cells, row-major diffing, <1ms diff target |
| **Integration Value** | High — provides rendering kernel, event model, layout engine |
| **Top Extraction** | Event, TerminalCapabilities, text_width, Backend trait, Cell |
| **Risk** | Low-medium — same toolchain, incremental adoption path |
| **Maturity** | Production-ready core, extensive widget library, comprehensive testing |
