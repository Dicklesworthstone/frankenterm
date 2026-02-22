# Plan to Deeply Integrate FrankenTUI into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.3.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_frankentui.md (ft-2vuw7.3.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Observation TUI dashboard**: Build an Elm-style observation dashboard using ftui-runtime Model trait + ftui-widgets for monitoring pane activity, workflow state, and system health
2. **Inline CLI status bar**: Use FrankenTUI's inline mode for `ft` CLI to show pane health indicators, workflow progress, and alerts while preserving scrollback
3. **Custom mux backend**: Implement `Backend` trait to render FrankenTUI apps into FrankenTerm mux panes
4. **Headless widget rendering**: Use ftui-render Buffer + ftui-widgets for formatted robot mode output (tables, sparklines, progress bars)
5. **Leverage existing PTY bridge**: ftui-pty already depends on frankenterm-core; deepen this integration

### Constraints

- **No async runtime conflict**: ftui-runtime is synchronous Elm-style; must coexist with FrankenTerm's async runtime
- **No unsafe code**: FrankenTUI uses `#![forbid(unsafe_code)]` across 404 files; this aligns with frankenterm-core policy
- **Edition compatibility**: Both use Rust 2024 edition
- **Binary size consideration**: Full FrankenTUI is ~400K LOC; use minimal crate subset initially
- **No feature regression**: Existing FrankenTerm search, pattern detection, and robot mode must continue working

### Non-Goals

- **Replacing WezTerm's UI**: FrankenTerm observes mux state, not replaces the multiplexer's own UI
- **Full ftui-extras inclusion**: Only include extras features actually needed (no charts/games/visual-fx in Phase 1)
- **WASM target**: ftui-web and ftui-showcase-wasm are not needed for FrankenTerm's server-side use
- **Text shaping**: ftui-text's optional rustybuzz shaping is not needed
- **GPU rendering**: ftui-extras' wgpu feature is not relevant

---

## P2: Evaluate Integration Patterns

### Option A: Full Runtime Integration (Chosen for Dashboard)

Use ftui-runtime's Model/Cmd/view cycle for the observation dashboard.

**Pros**: Elm architecture prevents state bugs, subscriptions for live updates, built-in frame timing
**Cons**: Adds ftui-runtime dependency (~66K LOC), synchronous event loop must coordinate with async FrankenTerm

### Option B: Headless Widget Rendering (Chosen for Robot Mode)

Use ftui-render Buffer + ftui-widgets without the runtime event loop.

**Pros**: Minimal dependency (~147K LOC), no event loop conflict, pure function input→output
**Cons**: No interactive features, manual buffer management

### Option C: Custom Backend Only

Only implement Backend trait, use FrankenTerm's own event loop.

**Pros**: Maximum control, minimal FrankenTUI surface
**Cons**: Must reimplement event loop, miss subscription/command patterns

### Decision: Options A+B Combined

- **Dashboard**: Full ftui-runtime (Option A) behind `tui-dashboard` feature
- **Robot mode**: Headless rendering (Option B) behind `tui-widgets` feature
- **Custom backend**: Implement Backend trait for mux integration (part of Option A)

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── tui_bridge.rs              # NEW: FrankenTerm types → ftui widget data
│   ├── tui_dashboard.rs           # NEW: ObservationDashboard Model
│   ├── tui_robot_render.rs        # NEW: Headless rendering for robot mode
│   ├── tui_mux_backend.rs         # NEW: Backend impl for mux panes
│   └── ...existing modules...
├── Cargo.toml                     # Add path deps: ftui-*, feature-gated
```

### Module Responsibilities

#### `tui_bridge.rs` — Data Conversion

Converts FrankenTerm internal types to ftui widget data without importing ftui-runtime:
- `panes_to_table_rows(panes: &[PaneInfo]) -> Vec<Row>` — Pane list for Table widget
- `metrics_to_sparkline(metrics: &[f64]) -> SparklineData` — Metrics for Sparkline
- `alerts_to_list_items(alerts: &[Alert]) -> Vec<ListItem>` — Alerts for List

#### `tui_dashboard.rs` — Observation Dashboard (requires `tui-dashboard`)

Full Elm-style dashboard implementing ftui_runtime::Model:
- Pane status table with live updates
- Metrics sparklines (CPU, memory, event rate)
- Alert notification panel
- Workflow progress indicators

#### `tui_robot_render.rs` — Headless Rendering (requires `tui-widgets`)

Renders formatted terminal output for robot mode without an event loop:
- `render_pane_table(panes: &[PaneInfo], cols: u16) -> String` — Formatted table
- `render_metrics_sparkline(metrics: &[f64], cols: u16) -> String` — Sparkline
- `render_workflow_progress(steps: &[WorkflowStep], cols: u16) -> String` — Progress

#### `tui_mux_backend.rs` — Mux Backend (requires `tui-dashboard`)

Implements ftui-backend traits to route FrankenTUI I/O through WezTerm mux:
- `MuxBackendEventSource` — reads from mux event channel
- `MuxBackendPresenter` — writes to mux pane grid
- `MuxBackendClock` — uses FrankenTerm's timing

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
tui-widgets = ["ftui-render", "ftui-layout", "ftui-style", "ftui-widgets"]
tui-dashboard = ["tui-widgets", "ftui-runtime", "ftui-core", "ftui-backend"]

[dependencies]
ftui-core = { path = "../../frankentui/crates/ftui-core", optional = true }
ftui-render = { path = "../../frankentui/crates/ftui-render", optional = true }
ftui-layout = { path = "../../frankentui/crates/ftui-layout", optional = true }
ftui-style = { path = "../../frankentui/crates/ftui-style", optional = true }
ftui-widgets = { path = "../../frankentui/crates/ftui-widgets", optional = true }
ftui-runtime = { path = "../../frankentui/crates/ftui-runtime", optional = true }
ftui-backend = { path = "../../frankentui/crates/ftui-backend", optional = true }
```

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: TuiBridge

```rust
#[cfg(feature = "tui-widgets")]
pub mod tui_bridge {
    use ftui_widgets::{Row, ListItem, Cell as WidgetCell};
    use ftui_style::Style;

    pub fn panes_to_table_rows(panes: &[PaneInfo]) -> Vec<Row>;
    pub fn metrics_to_sparkline_data(metrics: &[f64]) -> Vec<u64>;
    pub fn alerts_to_list_items(alerts: &[Alert]) -> Vec<ListItem>;
    pub fn workflow_steps_to_progress(steps: &[WorkflowStep]) -> (usize, usize);
    pub fn pane_style(pane: &PaneInfo) -> Style;  // Color based on state
}
```

### Public API Contract: TuiRobotRender

```rust
#[cfg(feature = "tui-widgets")]
pub mod tui_robot_render {
    pub fn render_pane_table(panes: &[PaneInfo], cols: u16) -> String;
    pub fn render_metrics_sparkline(label: &str, values: &[f64], cols: u16) -> String;
    pub fn render_workflow_progress(steps: &[WorkflowStep], cols: u16) -> String;
    pub fn render_alert_panel(alerts: &[Alert], cols: u16, rows: u16) -> String;
}
```

### Public API Contract: ObservationDashboard

```rust
#[cfg(feature = "tui-dashboard")]
pub mod tui_dashboard {
    use ftui_runtime::{Model, Cmd};
    use ftui_render::Frame;

    pub struct ObservationDashboard {
        panes: Vec<PaneInfo>,
        metrics: MetricsBuffer,
        alerts: Vec<Alert>,
        selected_pane: usize,
    }

    pub enum DashboardMsg {
        PanesUpdated(Vec<PaneInfo>),
        MetricsUpdated(ResourceSnapshot),
        AlertReceived(Alert),
        SelectPane(usize),
        Tick,
        Quit,
    }

    impl Model for ObservationDashboard {
        type Message = DashboardMsg;
        fn update(&mut self, msg: DashboardMsg) -> Cmd<DashboardMsg>;
        fn view(&self, frame: &mut Frame);
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: Path dependencies via `/dp/frankentui`
**Phase 2**: Version dependencies when published to crates.io
**Phase 3**: If tight coupling, extract `ft-tui` as frankenterm-specific wrapper crate

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — all modules are new capabilities:
- TUI dashboard is a new observation mode
- Robot mode rendering replaces plain text with formatted output
- No existing data formats change

### State Synchronization

- Dashboard receives state updates via subscriptions (push model)
- Robot mode renders are pure functions of current state (pull model)
- No bidirectional sync needed

### Compatibility Posture

- **Additive only**: All features behind `tui-widgets` / `tui-dashboard` feature gates
- **No regression**: Without features, behavior identical to current
- **Graceful degradation**: DegradationLevel enum handles terminal size constraints

---

## P6: Testing Strategy

### Unit Tests

#### `tui_bridge.rs` (target: 30+)
- Table row conversion for various pane states
- Sparkline data normalization
- Alert list item formatting
- Style mapping by pane state
- Edge cases: empty lists, Unicode labels, long text truncation

#### `tui_robot_render.rs` (target: 30+)
- Headless table rendering at various widths
- Sparkline rendering correctness
- Progress bar rendering
- Alert panel layout
- Column width adaptation

#### `tui_dashboard.rs` (target: 30+)
- Model update message handling
- View layout at various terminal sizes
- Subscription lifecycle
- State transitions

### Integration Tests

- `test_dashboard_model_update_cycle` — full update→view cycle
- `test_robot_render_pane_table` — PaneInfo → formatted string
- `test_feature_gate_disabled` — no regression when features disabled

### Property-Based Tests

- `proptest_table_rows_match_pane_count` — row count invariant
- `proptest_sparkline_bounds` — values within display range
- `proptest_render_fits_width` — output fits requested columns

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: Headless Widgets (Week 1-3)**
1. Add ftui-render, ftui-layout, ftui-style, ftui-widgets behind `tui-widgets` feature
2. Implement `tui_bridge.rs` and `tui_robot_render.rs`
3. Wire into `ft robot state` and `ft robot metrics` commands
4. 60+ unit tests

**Phase 2: Dashboard Runtime (Week 4-6)**
1. Add ftui-runtime, ftui-core, ftui-backend behind `tui-dashboard` feature
2. Implement `tui_dashboard.rs` with ObservationDashboard model
3. Implement `tui_mux_backend.rs` for mux integration
4. 30+ dashboard tests

**Phase 3: Inline CLI Mode (Week 7-8)**
1. Add inline mode to `ft` CLI using TerminalWriter
2. Status bar with pane health indicators
3. Integration tests

### Rollback Plan

- **Phase 1**: Remove `tui-widgets` feature, revert Cargo.toml
- **Phase 2**: Disable `tui-dashboard` feature
- All rollbacks are single-commit operations

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Compile time increase | High | Medium | Feature-gated, only compile when needed |
| Event loop conflict | Medium | High | Dashboard runs in separate thread, async bridge |
| FrankenTUI API changes | Low | Medium | Pin to specific commit |
| Memory overhead | Low | Low | Degradation levels manage complexity |
| Binary size | Medium | Low | Release profile already optimizes for size |

### Acceptance Gates

1. `cargo check --workspace --all-targets` with and without features
2. `cargo test --workspace` all existing tests pass
3. `cargo clippy -- -D warnings`
4. 60+ unit tests in tui_bridge + tui_robot_render
5. 30+ unit tests in tui_dashboard
6. Integration tests pass
7. No performance regression

---

## P8: Summary

### Chosen Architecture

**Dual-mode integration**: Headless widget rendering (tui-widgets feature) for robot mode output + full Elm-style dashboard (tui-dashboard feature) for observation UI. Custom Backend implementation routes I/O through mux.

### Four New Modules

1. **`tui_bridge.rs`**: FrankenTerm types → ftui widget data
2. **`tui_robot_render.rs`**: Headless formatted output for robot mode
3. **`tui_dashboard.rs`**: ObservationDashboard implementing Model trait
4. **`tui_mux_backend.rs`**: Backend impl for mux pane I/O

### Implementation Order

1. Add Cargo.toml dependencies (feature-gated)
2. Implement tui_bridge.rs (30+ tests)
3. Implement tui_robot_render.rs (30+ tests)
4. Wire into ft robot commands
5. Implement tui_dashboard.rs (30+ tests)
6. Implement tui_mux_backend.rs
7. Integration and property-based tests
8. Inline CLI mode

### Key Advantage

FrankenTUI is part of the same project ecosystem and `ftui-pty` already bridges to `frankenterm-core`. This is the most natural TUI framework choice — no external project alignment needed.

---

*Plan complete. Ready for review and implementation bead creation.*
