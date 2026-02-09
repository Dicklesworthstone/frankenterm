# ADR-004: FrankenTUI Dependency and Feature Strategy

**Status:** Accepted
**Date:** 2026-02-09
**Bead:** wa-1utb (FTUI-02.1)
**Epic:** wa-1k52 (FTUI-02)

## Context

wa needs to consume `/dp/frankentui` (ftui) as an optional dependency during
the TUI migration (ADR-001). The integration must allow both the legacy
ratatui stack and the new ftui stack to coexist in the dependency graph during
the migration period, while ensuring:

- Default builds are unaffected
- Feature flags are explicit and non-conflicting
- The dependency graph is deterministic and reproducible

## Decision

### Workspace Dependency

ftui is declared as a workspace dependency in the root `Cargo.toml`:

```toml
# Path dep during development; will switch to git pin per FTUI-02.2 sync policy.
# Pinned rev for reference: 65b8538 (2026-02-08)
ftui = { path = "../frankentui/crates/ftui", default-features = false, features = ["runtime"] }
```

During development, a path dependency is used for fast iteration. Before
release, this will switch to a git dependency pinned to a specific commit
(tracked in FTUI-02.2).

### Feature Flags

Two mutually exclusive TUI features exist in `crates/wa-core/Cargo.toml`:

| Feature | Dependencies | Purpose |
|---------|-------------|---------|
| `tui` | ratatui 0.30, crossterm 0.29 | Legacy TUI (current, stable) |
| `ftui` | ftui 0.1.1 | FrankenTUI migration target |

These features are **not activated by default**. Application code must opt in.

### Feature Matrix

| Build Configuration | Compiles | TUI Available |
|---------------------|----------|---------------|
| `cargo check` (default) | Yes | No |
| `cargo check --features tui` | Yes | ratatui |
| `cargo check --features ftui` | Yes | ftui |
| `cargo check --features tui,ftui` | Yes | Both deps available |

### Conditional Compilation

Migration code uses `cfg(feature = "ftui")` guards:

```rust
#[cfg(feature = "ftui")]
mod ftui_tui;

#[cfg(feature = "tui")]
mod tui;
```

During migration, both modules may exist. After migration is complete and
validated, the `tui` feature and ratatui dependency will be removed (FTUI-09.3).

## ftui Feature Selection

The workspace dep enables these ftui features:
- `runtime` — provides `App`, `Model`, `Program`, `ScreenMode` (required for
  the event loop and application shell)
- Default features disabled — no extras or crossterm-compat layer

The `crossterm` ftui feature is intentionally not enabled. ftui uses its own
terminal backend (`ftui-tty`), separate from crossterm.

## Consequences

- Default builds are completely unaffected (no new deps compiled)
- Both TUI stacks can coexist during migration without conflicts
- The `ftui` feature is independent from `tui` — no implicit coupling
- Path dependency enables rapid iteration during development
- Git pin will ensure reproducible builds before release

## References

- ADR-001: Migration Intent
- ADR-002: Migration Principles
- FTUI-02.2 (wa-e2jh): Upstream sync/pinning policy
- FTUI-02.3 (wa-8q4e): Temporary compatibility adapter
- FTUI-02.4 (wa-eutd): Build guardrails against dual-stack drift
