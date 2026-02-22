# Comprehensive Analysis of FrankenTUI

> Integration research for FrankenTerm bead `ft-2vuw7.3.1`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Status: Complete

---

## R1: Repository Topology and Crate/Module Boundary Inventory

### Overview

FrankenTUI is a full-stack terminal UI framework implementing an Elm/Bubbletea-style architecture. It provides everything from terminal I/O abstraction to a 48+ widget library with inline mode support, deterministic diff-based rendering, and WASM targets.

| Metric | Value |
|--------|-------|
| Total Rust LOC | 684,533 |
| Rust files | 609 |
| Workspace crates | 19 |
| Test functions | 20,881 |
| Benchmark files | 27 |
| Rust edition | 2024 |
| Unsafe code | Zero in production (`#![forbid(unsafe_code)]` on 404 files) |

### Workspace Structure

```
frankentui/
├── Core Stack (6 crates)
│   ├── ftui-core         (37 files, ~33K LOC) — Terminal I/O, events, capabilities
│   ├── ftui-render       (47 files, ~41K LOC) — Buffer, Cell, Diff, Presenter
│   ├── ftui-layout       (31 files, ~20K LOC) — Flex, Grid, constraint solvers
│   ├── ftui-text         (36 files, ~27K LOC) — Rope, text measurement, wrapping
│   ├── ftui-style        (8 files, ~8K LOC)   — Style, Color, Theme system
│   └── ftui-i18n         (4 files, ~1K LOC)   — Localization
├── Runtime & Widgets (3 crates)
│   ├── ftui-runtime      (67 files, ~66K LOC) — Elm-style event loop, subscriptions
│   ├── ftui-widgets      (93 files, ~78K LOC) — 48+ widgets with hit testing
│   └── ftui              (2 files, ~1K LOC)   — Public facade/prelude
├── Backend Abstraction (2 crates)
│   ├── ftui-backend      (1 file, ~500 LOC)   — Platform abstraction traits
│   └── ftui-tty          (2 files, ~3K LOC)   — Native Unix backend
├── Web/Platform (3 crates)
│   ├── ftui-web          (6 files, ~7K LOC)   — WASM backend
│   ├── ftui-pty          (8 files, ~9K LOC)   — PTY utilities (bridges frankenterm-core)
│   └── ftui-simd         (1 file, ~17 LOC)    — Reserved for SIMD experiments
├── Extended (2 crates)
│   ├── ftui-extras       (91 files, ~136K LOC) — Feature-gated plugins (charts, canvas, etc.)
│   └── ftui-demo-showcase (110 files, ~113K LOC) — Reference app
└── Testing (2 crates)
    ├── ftui-harness      (46 files, ~18K LOC) — Snapshot test harness
    └── doctor_frankentui (16 files, ~7K LOC)  — Diagnostics CLI
```

### Crate Dependency Graph

```
ftui-core (foundation — events, capabilities)
  ├── ftui-render (Buffer, Cell, Diff, Presenter)
  ├── ftui-style (Style, Color, Theme)
  ├── ftui-text (Rope, text measurement)
  ├── ftui-layout (Flex, Grid, constraints)
  ├── ftui-backend (Backend trait — platform abstraction)
  │   └── ftui-tty (Unix native backend)
  │   └── ftui-web (WASM backend)
  ├── ftui-runtime (Elm event loop, Model/Cmd)
  │   └── ftui-widgets (48+ widgets)
  │       └── ftui-extras (feature-gated plugins)
  └── ftui-pty (PTY utilities — bridges to frankenterm-core)
```

---

## R2: Build/Runtime/Dependency Map and Feature-Flag Matrix

### External Dependencies (Core Stack)

| Crate | Key External Dependencies |
|-------|--------------------------|
| **ftui-core** | `ahash`, `arc-swap`, `bitflags`, `unicode-display-width`, `unicode-segmentation`, `web-time`, optional: `signal-hook`, `crossterm`, `tracing` |
| **ftui-render** | `bumpalo` (arena alloc), `memchr`, `smallvec`, unicode deps |
| **ftui-layout** | `rustc-hash` |
| **ftui-text** | unicode deps, optional: `rustybuzz` (text shaping) |
| **ftui-style** | minimal (depends on ftui-render) |
| **ftui-runtime** | optional: `im` (persistent DS), `opentelemetry`, `serde` |
| **ftui-widgets** | optional: `regex`, `serde` |
| **ftui-pty** | `frankenterm-core` (with `ws-codec` feature) |

### Key Feature Flags

| Feature | Scope | Purpose |
|---------|-------|---------|
| `runtime` | ftui | Include Elm-style event loop |
| `extras` | ftui | Include ftui-extras add-ons |
| `crossterm` | ftui-core | Legacy Crossterm backend |
| `native-backend` | ftui-runtime | ftui-tty Unix backend |
| `render-thread` | ftui-runtime | Dedicated render thread |
| `state-persistence` | ftui-runtime/widgets | JSON widget state storage |
| `telemetry` | ftui-runtime | OpenTelemetry OTLP export |
| `hamt` | ftui-runtime | Persistent DS for undo (im crate) |
| `diagram` | ftui-extras | Diagram rendering (uses franken_mermaid) |
| `terminal-widget` | ftui-extras | Embedded terminal widget |
| `pty-capture` | ftui-extras | PTY capture integration |

### Build Profile

```toml
[profile.release]
opt-level = "z"        # Size optimization
lto = true             # Link-time optimization
codegen-units = 1      # Single codegen unit
panic = "abort"        # Abort on panic
strip = true           # Strip symbols
```

---

## R3: Public Surface Inventory

### Key Types & Traits

#### Terminal Lifecycle (ftui-core)

```rust
pub struct TerminalSession;      // RAII terminal setup/cleanup
pub struct TerminalCapabilities;  // Detected terminal features
pub enum Event { Key, Mouse, Resize, Paste, Focus, Clipboard }
pub struct KeyEvent { code, modifiers, kind }
pub enum KeyCode { Char, Enter, Escape, F(u8), ... }
pub struct MouseEvent { kind, x, y, modifiers }
```

#### Rendering (ftui-render)

```rust
pub struct Cell;                  // 16-byte fixed layout, SIMD-friendly
pub struct Buffer;                // Grid of Cells with dirty tracking
pub struct Frame;                 // Render surface with HitGrid, cursor
pub struct BufferDiff;            // Change detection for minimal ANSI
pub struct Presenter;             // ANSI emitter with state tracking
pub struct GraphemePool;          // Interned grapheme strings
```

#### Runtime (ftui-runtime)

```rust
pub trait Model {
    type Message: From<Event> + Send + 'static;
    fn init(&mut self) -> Cmd<Self::Message>;
    fn update(&mut self, msg: Self::Message) -> Cmd<Self::Message>;
    fn view(&self, frame: &mut Frame);
    fn subscriptions(&self) -> Vec<Box<dyn Subscription<Self::Message>>>;
}
pub enum Cmd<M> { None, Quit, Batch, Sequence, Msg, Tick, Log, ... }
pub struct Program<M: Model>;     // Manages event loop
pub struct App;                   // Builder methods
pub enum ScreenMode { AltScreen, Inline, InlineAuto }
pub struct TerminalWriter;        // Serializes log+UI writes
```

#### Layout (ftui-layout)

```rust
pub struct Flex;                  // 1D constraint solver
pub struct Grid;                  // 2D grid with cell spanning
pub enum Constraint { Fixed, Percentage, Min, Max, Ratio, FitContent, Fill }
```

#### Backend Abstraction (ftui-backend)

```rust
pub trait Backend {
    type Error; type Clock; type Events; type Presenter;
}
pub trait BackendEventSource { fn poll_event, fn read_event, fn size, fn set_features }
pub trait BackendPresenter { fn present_ui, fn write_log, fn capabilities }
pub trait BackendClock { fn now_mono }
```

#### Widgets (ftui-widgets) — 48+ Total

Layout: Block, Padding, Group, Columns, Rows, Tabs, Modal, Panel
Lists: List, Table, Tree, VirtualizedList, Paginator
Input: TextInput, TextArea, CommandPalette, FilePickerInputWidget
Viz: Paragraph, ProgressBar, Spinner, Sparkline, BarChart, LineChart
Interactive: Scrollbar, Draggable, DropTarget, FocusManager, HelpRegistry
Debug: LayoutDebugger, DebugOverlay
Specialized: LogViewer, NotificationQueue, Toast, Stopwatch, Timer, JsonView

---

## R4: Execution-Flow Tracing

### Main Pipeline

```
App::new(model).run()
  → Terminal Setup (TerminalSession: raw mode, alt screen, capabilities)
  → model.init() → Cmd
  → Loop:
      1. Poll events (timeout ~16ms)
      2. Process commands/subscriptions
      3. model.update(msg) → Cmd
      4. model.subscriptions() → start/stop subs
      5. model.view(&mut frame) → widgets draw into Buffer
      6. BufferDiff::compute(prev, new) → minimal changes
      7. Presenter.present_ui(buf, diff) → ANSI to terminal
      8. write_log() if inline mode
  → Until Cmd::Quit
  → Terminal Teardown (Drop: restore modes, cursor, cleanup)
```

### Inline Mode (Unique Feature)

FrankenTUI can render a fixed-height UI at the bottom of the terminal while preserving scrollback above. `TerminalWriter` handles cursor save/restore and atomic writes, preventing UI corruption during concurrent log output.

### Hit Testing

Widgets register clickable regions via `frame.register_hit(area, id, region, data)`. The HitGrid maps (x,y) coordinates to widget IDs for mouse interaction. This enables row-click in tables, button press, scrollbar drag, etc.

---

## R5: Data/State/Persistence Contract Analysis

### State Management

- **Elm architecture**: Immutable Model + Message + Update + View
- Messages drive all state changes (no shared mutable state)
- Commands return async effects (ticks, IO, quit)
- Subscriptions provide ongoing event streams

### Memory Management

- **Cell**: 16 bytes fixed layout (4 per cache line) — SIMD-friendly
- **GraphemePool**: Interned strings, 31-bit ID (16-bit slot + 11-bit generation + 4-bit width)
- **Arena allocation**: `bumpalo` for temporary frame objects, batch-freed at frame boundary
- **Dirty span tracking**: Buffer tracks changed regions for minimal diff

### Persistence (Optional)

- `state-persistence` feature: JSON snapshots of widget state
- `PersistenceConfig`: File or custom storage backend
- Supports: TextInput cursor, List selection, Table sort order

---

## R6: Reliability/Performance/Security

### Error Handling

- Rich error enum: `Error { Io, Terminal, Render, Layout, Protocol, Widget }`
- RAII cleanup: TerminalSession restores terminal even on panic
- Graceful degradation: `DegradationLevel { Full, Reduced, Essential, Panic }`

### Safety

- `#![forbid(unsafe_code)]` in 404 files (all production code)
- Zero unsafe blocks in source code
- Platform code gated behind `#[cfg]` attributes

### Performance

- 16-byte Cell layout for SIMD-friendly buffer operations
- Row-major dirty span tracking for O(min(area, changes)) diff
- Grapheme interning avoids redundant allocations
- Presenter maintains cursor/mode state to minimize ANSI escapes
- 60 FPS target with evidence-based frame budget adaptation
- 27 criterion benchmarks for regression detection

---

## R7: Integration Seam Discovery

### FrankenTerm Integration Candidates

#### 1. Observation TUI Dashboard (PRIMARY)

FrankenTerm needs a terminal UI for monitoring pane activity, workflow state, and system health. FrankenTUI provides the widget library (Table, List, Sparkline, ProgressBar), layout system (Flex), and inline mode.

**Integration path**: Use `ftui-runtime` Model trait + `ftui-widgets`

```rust
struct ObservationDashboard {
    panes: Vec<PaneInfo>,
    active_alerts: Vec<Alert>,
    metrics: ResourceSnapshot,
}

impl Model for ObservationDashboard {
    type Message = DashboardMsg;
    fn view(&self, frame: &mut Frame) {
        // Pane list + metrics sparklines + alert panel
    }
}
```

#### 2. Custom Backend for Mux Integration (HIGH VALUE)

Implement `BackendEventSource` and `BackendPresenter` for FrankenTerm's mux, enabling FrankenTUI apps to render into mux panes.

```rust
struct MuxBackend { /* routes I/O through WezTerm mux */ }
impl Backend for MuxBackend { ... }
```

#### 3. Widget Reuse for Robot Mode Output (MODERATE)

Use `ftui-render::Buffer` + `ftui-widgets` headlessly to generate formatted terminal output for `ft robot` commands:
- Tables for `ft robot state`
- Sparklines for `ft robot metrics`
- Progress bars for `ft robot workflow`

#### 4. Inline Mode for ft CLI (HIGH VALUE)

FrankenTerm's CLI could use inline mode to show a status bar while preserving scrollback:
- Pane health indicators
- Workflow progress
- Alert notifications

#### 5. PTY Bridge Already Exists

`ftui-pty` already depends on `frankenterm-core` with `ws-codec` feature for WebSocket-based PTY multiplexing. This is an existing integration point.

### Dependency Cost

**Minimal (headless rendering only)**:
```toml
ftui-render = "0.2"   # 41K LOC — Buffer, Cell, Diff
ftui-layout = "0.2"   # 20K LOC — Flex, Grid
ftui-style = "0.2"    # 8K LOC — Style, Color
ftui-widgets = "0.2"  # 78K LOC — 48+ widgets
# Total: ~147K LOC, zero async deps
```

**Full runtime**:
```toml
ftui = { version = "0.2", features = ["runtime"] }
# Total: ~400K LOC including event loop
```

### Strategic Fit Assessment

| Criterion | Score | Rationale |
|-----------|-------|-----------|
| Architectural alignment | 10/10 | Same project ecosystem, shared types via ftui-pty |
| Dependency cost | 6/10 | 147K-400K LOC is substantial but justified for TUI |
| API cleanliness | 9/10 | Elm-style Model/Cmd is clean and well-structured |
| Performance fit | 9/10 | 16-byte Cells, dirty tracking, SIMD-friendly |
| Feature relevance | 9/10 | Inline mode, widgets, hit testing all directly useful |
| Maintenance burden | 7/10 | Large codebase but same project ecosystem |
| **Overall** | **8.3/10** | Natural TUI framework for FrankenTerm |

### Comparison to Ratatui

| Feature | FrankenTUI | Ratatui |
|---------|-----------|---------|
| Inline mode | First-class | App-specific |
| Deterministic diff | Kernel-level | Application-level |
| One-writer guarantee | Enforced | App-specific |
| RAII cleanup | Built-in | External |
| Widget count | 48+ | 20+ |
| Backend abstraction | Full trait | Crossterm-only |
| Memory layout | 16-byte Cell | Variable |
| Test harness | Built-in snapshot | External |

---

## R8: Research Evidence Pack

### Evidence Summary

| Area | Status | Key Finding |
|------|--------|-------------|
| R1: Topology | Complete | 19 crates, 684K LOC, 609 files |
| R2: Build/deps | Complete | Layered deps, extensive feature flags |
| R3: Public API | Complete | Model/Cmd pattern, 48+ widgets, Backend trait |
| R4: Execution flows | Complete | Elm loop, inline mode, hit testing |
| R5: Data/persistence | Complete | Immutable model, arena allocation, optional persistence |
| R6: Reliability | Complete | forbid(unsafe_code), RAII, degradation levels |
| R7: Integration seams | Complete | Dashboard, custom backend, headless rendering, inline mode |

---

*Analysis complete. Ready for integration plan creation.*
