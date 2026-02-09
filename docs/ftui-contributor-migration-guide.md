# Contributor Migration Guide: ratatui → FrankenTUI

**Bead:** wa-83us (FTUI-09.1)
**Date:** 2026-02-09

---

## 1  Overview

wa is migrating its TUI backend from ratatui+crossterm to FrankenTUI (ftui). Both backends coexist behind mutually exclusive Cargo features (`tui` vs `ftui`) during the migration period. This guide covers what contributors need to know to work in the codebase during and after the transition.

## 2  Feature Flags

| Feature | Backend | Status |
|---------|---------|--------|
| `tui` | ratatui + crossterm (legacy) | Production, will be decommissioned |
| `ftui` | FrankenTUI | Migration target, will become default |

These features are **mutually exclusive** — enabling both causes a `compile_error!` (enforced in `wa-core/src/lib.rs`).

### Building and Testing

```bash
# Headless (no TUI, robot/CLI/MCP only)
cargo check -p wa-core
cargo test -p wa-core

# Legacy TUI
cargo check -p wa-core --features tui
cargo test -p wa-core --features tui --lib

# FrankenTUI (migration target)
cargo check -p wa-core --features ftui
cargo test -p wa-core --features ftui --lib

# Full binary
cargo check -p wa --features ftui,mcp,web,metrics
```

See `docs/ftui-cargo-feature-matrix.md` for the complete feature matrix.

## 3  Code Organization

### TUI Module Layout

```
crates/wa-core/src/tui/
├── mod.rs              # cfg-gated re-exports
├── terminal_session.rs # Shared: alt-screen, raw mode, SessionGuard
├── command_handoff.rs  # Shared: suspend/resume for subprocess exec
├── output_gate.rs      # Shared: atomic gate suppressing tracing output
├── crash.rs            # Shared: panic handler with terminal restoration
├── keymap.rs           # ftui: key binding configuration
├── state.rs            # ftui: ViewState, FocusRegion, per-view state
├── view_adapters.rs    # ftui: domain→view-model adapters
└── ftui_stub.rs        # ftui: WaModel, views, rendering, tests
```

### Shared vs Backend-Specific Code

| Module | Scope | Notes |
|--------|-------|-------|
| `terminal_session.rs` | Shared | SessionGuard RAII, alt-screen, raw mode |
| `command_handoff.rs` | Shared | CommandHandoff state machine |
| `output_gate.rs` | Shared | GatePhase atomic, TuiAwareWriter |
| `crash.rs` | Shared | Panic hook with terminal restoration |
| `ftui_stub.rs` | ftui only | Model, views, message routing |
| `view_adapters.rs` | ftui only | Adapter functions for each view |
| `state.rs` | ftui only | Per-view state structs |
| `keymap.rs` | ftui only | Key binding registry |

## 4  Architecture Differences

### Message-Based Architecture (ftui)

ftui uses a TEA (The Elm Architecture) pattern:

```
Event → update(WaMsg) → Cmd<WaMsg> → view(Frame)
```

Key types:
- `WaMsg` — message enum: `TermEvent(Event)`, `SwitchView(View)`, `NextTab`, `PrevTab`, `Tick`, `Quit`
- `WaModel` — application state + `ftui::Model` implementation
- `View` — enum of 7 views: Home, Panes, Events, Triage, History, Search, Help

### Key Routing

```
TermEvent(Key) → handle_modal_key() → handle_global_key() → handle_view_key()
```

1. Modal intercept (confirmation dialogs absorb all keys)
2. Global: Tab/BackTab navigation, 'q' quit, 'r' refresh, digit view shortcuts
3. View-specific: per-view navigation, filter input, selection

Digit keys 1-7 switch views globally **except** on Events/Triage/History views where they're routed to view-specific handlers (filter input).

### Data Flow

```
MockQuery/ProductionQueryClient → refresh_data() → model fields → view()
```

Data is pulled on `Tick` and `'r'` keypress. Views read directly from model fields (panes, triage_items, events, etc.).

## 5  Adding a New View

1. Add variant to `View` enum in `ftui_stub.rs`
2. Update `View::all()`, `View::next()`, `View::prev()`, `View::from_shortcut()`
3. Add view state struct to `state.rs` and field to `ViewState`
4. Add `handle_<view>_key()` method on `WaModel`
5. Add rendering function and wire into `WaModel::view()`
6. Add view adapter in `view_adapters.rs`
7. Add tests: unit (adapters), snapshot (rendering), E2E (key routing)

## 6  Adding Tests

### Test Infrastructure

| Helper | Location | Purpose |
|--------|----------|---------|
| `make_model(query)` | ftui_stub.rs tests | Create WaModel with MockQuery |
| `press_key(model, code)` | ftui_stub.rs tests | Direct key press (no update pipeline) |
| `E2eSession` | ftui_stub.rs tests | Full pipeline: press/char/capture/assert_view |
| `MockQuery` | ftui_stub.rs tests | Fixtures: healthy/degraded/with_events/with_history/with_triage |
| `frame_to_text(frame)` | ftui_stub.rs tests | Render frame to string for assertions |
| `assert_ti(ti, text, cursor)` | ftui_stub.rs tests | TextInput state assertion |

### Test Categories

| Category | Pattern | Feature | Example |
|----------|---------|---------|---------|
| Adapter unit | `fixture_*` | ftui | `fixture_events_view_all_variants` |
| Snapshot/golden | `snapshot_*` | ftui | `snapshot_home_80x24` |
| E2E headless | `e2e_*` | ftui | `e2e_tab_cycles_all_views` |
| Edge cases | `edge_*` | ftui | `edge_multibyte_insert_and_delete` |
| Chaos/resilience | `chaos_*` | ftui | `chaos_resize_during_key_storm` |
| Terminal session | Various | tui | `test_session_lifecycle` |
| Command handoff | `trace_*`, `invariant_*` | tui | `trace_nominal_suspend_resume` |
| Output gate | Various | tui | `test_gate_phase_transitions` |

### Creating Key Events in Tests

```rust
// Using E2eSession (recommended for integration tests)
let mut s = E2eSession::new(MockQuery::with_events());
s.char('3');  // switch to Events
s.press(ftui::KeyCode::Tab);
s.capture();
s.assert_view(View::Panes);

// Using press_key helper (for focused unit tests)
let mut model = make_model(MockQuery::healthy());
press_key(&mut model, ftui::KeyCode::Down);

// Manual key construction (for custom modifiers)
let key = ftui::KeyEvent {
    code: ftui::KeyCode::Char('q'),
    kind: ftui::KeyEventKind::Press,
    modifiers: ftui::Modifiers::empty(),
};
model.update(WaMsg::TermEvent(ftui::Event::Key(key)));
```

## 7  Behavioral Changes for Operators

### What Changes

| Behavior | ratatui (legacy) | ftui (new) | Impact |
|----------|-----------------|------------|--------|
| Key bindings | Hardcoded | Configurable via keymap.rs | Low — defaults match legacy |
| View navigation | Number keys only | Tab + number keys | Low — Tab added as alternative |
| Filter input | N/A | Digit filter on Events/Triage | New feature |
| Color rendering | crossterm colors | ftui colors | Visual — colors should match |
| Terminal restoration | Manual cleanup | SessionGuard RAII + panic hook | Improved reliability |
| Subprocess handoff | Direct | CommandHandoff state machine | More robust |

### What Stays the Same

- All 7 views: Home, Panes, Events, Triage, History, Search, Help
- 'q' to quit, '?' for help, 'r' to refresh
- j/k and arrow key navigation
- Robot Mode API (unaffected — headless)
- Configuration file format
- CLI arguments

### Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `compile_error!` about mutual exclusion | Both `tui` and `ftui` enabled | Use one feature at a time |
| No TUI rendered | Neither `tui` nor `ftui` enabled | Add `--features ftui` |
| Terminal not restored after crash | Panic handler not installed | Ensure `crash::install_panic_hook()` called |
| Colors wrong in tmux | `$TERM` set to `screen` | Set `default-terminal "tmux-256color"` |
| Keys not working in zellij | Key capture conflict | Check zellij keybinding config |

## 8  Evidence and References

### Parity Matrices

| View | Evidence | Status |
|------|----------|--------|
| Home | `evidence/ftui-05.2/matrix.md` | Complete |
| Panes | `evidence/ftui-05.3/matrix.md` | Complete |
| Events | `evidence/ftui-05.4/matrix.md` | Complete |
| Triage | `evidence/ftui-05.5/matrix.md` | Complete |
| History/Search/Help | `evidence/ftui-05.6/matrix.md` | Complete |

### Technical Documentation

- `docs/ftui-cargo-feature-matrix.md` — Feature flag matrix
- `docs/ftui-output-sink-routing.md` — Output routing contract
- `docs/ftui-subprocess-forwarding-contract.md` — PTY capture architecture
- `docs/ftui-teardown-harness.md` — Panic/abort restoration invariants
- `docs/ftui-command-handoff-traces.md` — Handoff state machine traces
- `docs/ftui-pty-fixture-strategy.md` — Deterministic test fixtures
- `docs/ftui-pty-failure-artifacts.md` — Failure artifact schema
- `docs/ftui-compat-runbook-template.md` — Per-environment certification template

### Test Reports

- `evidence/ftui-06.4/matrix.md` — TextInput edge-case matrix (20 cases)
- `evidence/ftui-08.3/compatibility-matrix.md` — Compatibility certification
- `evidence/ftui-08.4/stability-report.md` — Chaos/resilience validation

### Current Test Counts (2026-02-09)

| Suite | Count |
|-------|-------|
| View adapters | 76 |
| Snapshot/golden | 75 |
| E2E headless | 38 |
| TextInput edge cases | 21 |
| Chaos/resilience (ftui) | 20 |
| Terminal session | 50 |
| Command handoff | 19 |
| Output gate | 21 |
| **Total** | **320** |
