# Cargo Feature Matrix for FTUI Migration

**Bead:** wa-3g47 (FTUI-02.1.a)
**Date:** 2026-02-09
**Crate boundary:** `wa-core` (library), `wa` (binary)

## 1  Canonical Feature Combinations

### Supported Modes

| Mode | Features | Purpose | CI Gate |
|------|----------|---------|---------|
| **Headless** | (none) | Robot/CLI/MCP: no UI deps compiled | `cargo check -p wa-core` |
| **Legacy TUI** | `tui` | Production ratatui+crossterm backend | `cargo check -p wa-core --features tui` |
| **FrankenTUI** | `ftui` | Migration target ftui backend | `cargo check -p wa-core --features ftui` |
| **Legacy test** | `tui` | Unit + integration tests for legacy | `cargo test -p wa-core --features tui` |
| **ftui test** | `ftui` | Unit + integration tests for ftui | `cargo test -p wa-core --features ftui` |
| **Full legacy** | `tui,mcp,web,metrics` | Legacy binary with all optional frontends | `cargo check -p wa --features tui,mcp,web,metrics` |
| **Full ftui** | `ftui,mcp,web,metrics` | ftui binary with all optional frontends | `cargo check -p wa --features ftui,mcp,web,metrics` |

### Rollout Mode (Stages 1-2)

| Mode | Features | Purpose | CI Gate |
|------|----------|---------|---------|
| **Rollout** | `rollout` | Both backends compiled; runtime selection via `WA_TUI_BACKEND` | `cargo check -p wa-core --features rollout` |

The `rollout` feature implies `tui` + `ftui` and bypasses the mutual-exclusion
guard.  Backend selection happens at runtime via `tui::select_backend()`.

### Disallowed Combinations

| Combination | Enforcement | Error Message |
|-------------|-------------|---------------|
| `tui` + `ftui` (without `rollout`) | `compile_error!` in `wa-core/src/lib.rs` | "Features `tui` and `ftui` are mutually exclusive... Use `--features rollout` for runtime backend selection during migration." |

### Orthogonal Features

These features are independent of the TUI backend choice and may be freely
combined with any supported mode:

| Feature | Dependencies | Notes |
|---------|-------------|-------|
| `vendored` | codec, config, mux, wezterm-term | Native WezTerm integration |
| `browser` | (none, internal flag) | Browser automation auth flows |
| `mcp` | fastmcp | MCP server protocol |
| `web` | fastapi, fastapi-core, asupersync | HTTP API |
| `metrics` | (none, internal flag) | Prometheus/metrics collection |
| `native-wezterm` | (none, internal flag) | WezTerm native mode |
| `distributed` | rustls, tokio-rustls, rustls-pemfile, x509-parser | Distributed/TLS support |
| `sync` | asupersync | Sync infrastructure |

## 2  Compile-Check Plan

### Per-Commit CI Checks

Each CI run must verify these compile combinations deterministically:

```bash
# 1. Headless (no features) — must always compile
cargo check -p wa-core
cargo test -p wa-core

# 2. Legacy TUI — must compile and pass tests until tui feature is removed
cargo check -p wa-core --features tui
cargo test -p wa-core --features tui

# 3. FrankenTUI — must compile and pass tests
cargo check -p wa-core --features ftui
cargo test -p wa-core --features ftui

# 4. Mutual exclusion — must fail to compile
cargo check -p wa-core --features tui,ftui 2>&1 | grep -q "mutually exclusive"

# 5. Binary crate — both backends
cargo check -p wa --features tui
cargo check -p wa --features ftui

# 6. Clippy for both backends
cargo clippy -p wa-core --features tui -- -D warnings
cargo clippy -p wa-core --features ftui -- -D warnings
```

### Deterministic Failure Reporting

Each check logs:

```
[feature-matrix] mode=headless features=[] target=check result=PASS
[feature-matrix] mode=legacy   features=[tui] target=test result=PASS (2741 tests)
[feature-matrix] mode=ftui     features=[ftui] target=test result=PASS (3164 tests)
[feature-matrix] mode=conflict features=[tui,ftui] target=check result=FAIL (expected)
```

## 3  Feature Gate Inventory

### Shared code (always compiled, no feature gate)

| Module | Purpose |
|--------|---------|
| `tui::query` | QueryClient trait + data types |
| `tui::view_adapters` | Raw data → render-ready row models |
| `tui::ftui_compat` | Framework-agnostic types (StyleSpec, KeyInput, etc.) |
| `tui::keymap` | Canonical keybinding table |
| `tui::state` | Deterministic UI state reducer |
| `tui::output_gate` | One-writer atomic phase gate |
| `tui::terminal_session` | Session lifecycle abstraction |
| `tui::command_handoff` | Suspend/resume state machine |

### `cfg(feature = "tui")` code

| Module | Purpose |
|--------|---------|
| `tui::app` | Legacy ratatui event loop + App struct |
| `tui::views` | Legacy ratatui widget rendering |
| `tui::ftui_compat` conversions | `StyleSpec → ratatui::Style`, `Key → crossterm::KeyCode` |
| `tui::terminal_session` backends | `CrosstermBackend` session impl |

### `cfg(feature = "ftui")` code

| Module | Purpose |
|--------|---------|
| `tui::ftui_stub` | ftui Model implementation + all views |
| `tui::ftui_compat` conversions | `StyleSpec → ftui::CellStyle`, `Key → ftui::Key` |

### `cfg(any(feature = "tui", feature = "ftui"))` code

| Location | Purpose |
|----------|---------|
| `lib.rs:136` | `pub mod tui` (TUI module only compiled when either backend is active) |
| `crash.rs:215` | Gate-aware panic hook (checks output gate) |
| `logging.rs:172+` | TUI-aware log routing (consults output gate) |

## 4  Disallowed Combination Policy

### Current: `tui` + `ftui`

The `compile_error!` at `wa-core/src/lib.rs` fires when both features are active
**without** the `rollout` feature.  This is the only disallowed combination.

**Name collision handling:**  Both backends export `App`, `AppConfig`, `View`,
`ViewState`, and `run_tui` from `tui/mod.rs`.  Under `rollout`, these conflicting
re-exports are suppressed via `#[cfg(not(feature = "rollout"))]` guards, and the
`tui::rollout` dispatch module provides unified exports instead.

### Future: Removing `tui`

When FTUI-09.3 (decommission) is reached:

1. Remove `tui` and `rollout` features from `wa-core/Cargo.toml` and `wa/Cargo.toml`
2. Remove `compile_error!` guard and `rollout.rs` dispatch module
3. Remove all `#[cfg(feature = "tui")]` blocks
4. Rename `ftui` to `tui` (or make ftui the default)
5. Update CI matrix to drop legacy and rollout checks

Until then, all three modes (`tui`, `ftui`, `rollout`) must compile independently
and CI must verify all.

## 5  Test Mode Matrix

| Test category | Headless | Legacy (`tui`) | ftui (`ftui`) |
|---------------|----------|-----------------|---------------|
| Query/adapter unit tests | Yes | Yes | Yes |
| State reducer tests | Yes | Yes | Yes |
| Keymap tests | Yes | Yes | Yes |
| View rendering tests | N/A | ratatui widgets | ftui frame buffer |
| Snapshot/golden tests | N/A | N/A | ftui `frame_to_text()` |
| PTY E2E tests | N/A | crossterm PTY | ftui PTY |
| Output gate tests | Yes | Yes | Yes |
| Terminal session tests | Yes | Yes | Yes |
| Command handoff tests | Yes | Yes | Yes |
| ftui_compat conversion tests | N/A | ratatui→StyleSpec | ftui→StyleSpec |

"Yes" = test runs and passes in that mode.
"N/A" = test is not compiled (behind opposite feature gate).

## References

- `crates/wa-core/Cargo.toml:113-135` — feature definitions
- `crates/wa/Cargo.toml:40-51` — binary feature passthrough
- `crates/wa-core/src/lib.rs:129-134` — mutual exclusion guard
- ADR-0004: Phased Rollout and Rollback
- wa-36xw (FTUI-07.4): CI gate wiring (consumes this matrix)
- wa-1uqi (FTUI-03.2.a): Output sink routing (blocked by this)
