# Architecture Ring Map for FTUI Migration

**Bead:** wa-2xh0 (FTUI-01.2)
**Date:** 2026-02-09

## Ring Model

wa's modules are organized into concentric rings with strict dependency direction:
outer rings depend on inner rings, never the reverse. The ftui migration affects
only Ring 3 (Presentation). All other rings are untouched.

```
┌──────────────────────────────────────────────────────────────────────┐
│  Ring 4: Application (wa crate — main.rs CLI binary)                │
│  ┌────────────────────────────────────────────────────────────────┐  │
│  │  Ring 3: Presentation (TUI, Robot Types, MCP, Web)            │  │
│  │  ┌──────────────────────────────────────────────────────────┐  │  │
│  │  │  Ring 2: Logic (Patterns, Events, Workflows, Policy)    │  │  │
│  │  │  ┌────────────────────────────────────────────────────┐  │  │  │
│  │  │  │  Ring 1: Data (Storage, WezTerm IPC, Ingest)      │  │  │  │
│  │  │  │  ┌──────────────────────────────────────────────┐  │  │  │  │
│  │  │  │  │  Ring 0: Core (Types, Config, Errors)        │  │  │  │  │
│  │  │  │  └──────────────────────────────────────────────┘  │  │  │  │
│  │  │  └────────────────────────────────────────────────────┘  │  │  │
│  │  └──────────────────────────────────────────────────────────┘  │  │
│  └────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────┘
```

## Ring 0: Core (Types, Config, Errors)

Foundation types with no dependencies on wa business logic.

| Module | Responsibility |
|--------|---------------|
| `config` | Configuration parsing and validation |
| `config_profiles` | Named configuration profile sets |
| `error` | Error types and Result alias |
| `error_codes` | Stable error code registry |
| `logging` | Log routing and formatting |
| `output` | Output format abstraction (JSON, TOON) |
| `lock` | File-based locking primitives |
| `runtime` | Async runtime setup |
| `cleanup` | Resource cleanup utilities |
| `environment` | Environment detection (shell, agents, system) |

**Dependency rule:** Ring 0 modules depend only on external crates (serde, tokio,
clap, etc.) and each other. Never on Ring 1+.

## Ring 1: Data (Storage, WezTerm IPC, Ingest)

Data access and I/O layer. Reads from/writes to WezTerm and SQLite.

| Module | Responsibility |
|--------|---------------|
| `wezterm` | WezTerm CLI client wrapper (`wezterm cli list/get-text`) |
| `pool` | Connection pooling for WezTerm CLI |
| `storage` | SQLite storage with FTS5 search |
| `storage_targets` | Multi-target storage routing |
| `ingest` | Pane output capture and delta extraction |
| `tailer` | Real-time output tailing |
| `screen_state` | Terminal screen state tracking |
| `backup` | Database backup management |
| `export` | Data export (JSONL/NDJSON) |
| `recording` | Session recording to disk |
| `replay` | Session replay from recording |

**Dependency rule:** Ring 1 depends on Ring 0. Never on Ring 2+.

## Ring 2: Logic (Detection, Workflows, Policy)

Business logic that operates on captured data.

| Module | Responsibility |
|--------|---------------|
| `patterns` | Pattern detection engine (regex, anchors) |
| `rulesets` | Ruleset profiles and pack management |
| `events` | Event bus for detections and signals |
| `event_templates` | Human-readable event summaries |
| `workflows` | Durable workflow execution |
| `plan` | ActionPlan/StepPlan types |
| `undo` | Undo/redo framework for workflow actions |
| `dry_run` | Dry-run preview for all actions |
| `policy` | Safety gates and rate limiting |
| `approval` | Allow-once approval tokens |
| `secrets` | Secret detection and redaction |
| `circuit_breaker` | Circuit breaker for failed services |
| `degradation` | Graceful degradation modes |
| `crash` | Crash loop detection and backoff |
| `watchdog` | Watcher health monitoring |
| `backpressure` | Backpressure management |
| `wait` | Condition-based wait utilities |
| `retry` | Retry with exponential backoff |
| `accounts` | Account management and selection |
| `session_correlation` | Cross-pane event correlation |
| `suggestions` | Context-aware suggestion engine |
| `explanations` | Reusable explanation templates |
| `alerts` | Alert rule evaluation |
| `notifications` | Notification routing |
| `desktop_notify` | Desktop notification delivery |
| `email_notify` | Email notification delivery |
| `webhook` | Webhook delivery |
| `diagnostic` | Diagnostic data collection |
| `incident_bundle` | Incident bundle packaging |
| `cass` | CASS integration |
| `caut` | CAUT integration |
| `learn` | Interactive tutorial engine |
| `docs_gen` | Documentation generation |
| `reports` | Report generation |

**Dependency rule:** Ring 2 depends on Ring 0 + Ring 1. Never on Ring 3+.

## Ring 3: Presentation (TUI, Robot, MCP, Web)

User-facing surfaces that consume logic and data.

| Module | Feature Flag | Responsibility |
|--------|-------------|---------------|
| `tui` | `tui` | Interactive TUI (currently ratatui; **ftui migration target**) |
| `ui_query` | always | Shared query types for TUI views |
| `robot_types` | always | Robot mode response types |
| `api_schema` | always | API schema definitions |
| `mcp` | `mcp` | MCP server (stdio transport) |
| `web` | `web` | Optional HTTP server |
| `distributed` | `distributed` | Distributed mode networking |
| `simulation` | always | Mock WezTerm for testing |
| `extensions` | always | Extension/plugin system |
| `ipc` | unix only | IPC server for inter-process queries |

**Dependency rule:** Ring 3 depends on Ring 0 + Ring 1 + Ring 2. Never on Ring 4.

**FTUI migration scope:** Only the `tui` module in Ring 3 changes. All other Ring 3
modules are unaffected.

## Ring 4: Application (`wa` crate)

CLI binary that wires everything together.

| Component | Responsibility |
|-----------|---------------|
| `main.rs` (~30k lines) | CLI argument parsing, command dispatch, feature orchestration |

**Dependency rule:** Ring 4 depends on all inner rings. Nothing depends on Ring 4.

## Feature Flag Matrix

| Feature | Ring 3 Module | Dependencies Added |
|---------|--------------|-------------------|
| `tui` | `tui` | ratatui 0.30, crossterm 0.29 |
| `ftui` (new) | `tui` | frankentui (git pin) |
| `mcp` | `mcp` | fastmcp |
| `web` | `web` | fastapi, fastapi-core, asupersync |
| `vendored` | `vendored`, `wezterm_native` | codec, config, mux, wezterm-term |
| `browser` | `browser` | (none currently) |
| `metrics` | `metrics` | (none currently) |
| `distributed` | `distributed` | rustls, ring, base64 |
| `sync` | `sync` | asupersync |
| `native-wezterm` | `native_events` | (none currently) |

**Mutual exclusion:** `tui` and `ftui` must not be active simultaneously.
Enforced via `compile_error!` in `lib.rs`.

## Ownership Matrix

| Responsibility | Current Owner | Post-Migration Owner |
|---------------|---------------|---------------------|
| Terminal raw mode | `tui/app.rs` (crossterm) | ftui runtime |
| Alternate screen | `tui/app.rs` (crossterm) | ftui runtime |
| Render loop | `tui/app.rs` (`terminal.draw`) | ftui buffer/diff/present |
| Input handling | `tui/app.rs` (crossterm events) | ftui event loop |
| View rendering | `tui/views.rs` (ratatui widgets) | ftui widgets |
| Data queries | `tui/query.rs` (QueryClient) | `tui/query.rs` (unchanged) |
| Log routing | implicit (stderr) | ftui-managed output sink |
| Command handoff | `tui/app.rs` `run_command` | ftui suspend/resume protocol |
| Cursor management | `tui/app.rs` (crossterm) | ftui runtime |
| Signal handling | none (currently) | ftui cleanup hook |
| Panic recovery | none (terminal may be left raw) | ftui panic hook |

## Boundary Rules for ftui Code

1. **All ftui imports live in `crates/wa-core/src/tui/`** (Ring 3).
   No ftui types appear in Ring 0, 1, or 2.

2. **`ui_query` stays framework-agnostic.** The shared query types (`PaneView`,
   `EventView`, etc.) in `ui_query.rs` use plain Rust types. They are consumed
   by both the legacy TUI and the new ftui implementation.

3. **The `QueryClient` trait is the data boundary.** Views call `QueryClient`
   methods. They never import storage, wezterm, or pattern types directly.

4. **Feature flags gate compilation, not runtime.** There is no runtime
   feature detection. The binary is compiled with either `tui` or `ftui`.

5. **Ring 4 (main.rs) dispatches to Ring 3.** The CLI binary calls `run_tui()`
   which is implemented differently behind `tui` vs `ftui`, but the call site
   in main.rs is identical.

## References

- wa-core lib.rs module declarations: `crates/wa-core/src/lib.rs`
- Feature definitions: `crates/wa-core/Cargo.toml:110-131`
- ADR-0001: FrankenTUI adoption decision
- ADR-0002: One-writer terminal ownership
- ADR-0003: Migration scope and constraints
- Downstream: wa-136q (FTUI-01.3 Parity Contract)
