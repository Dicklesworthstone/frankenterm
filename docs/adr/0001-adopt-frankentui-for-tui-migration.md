# ADR-0001: Adopt FrankenTUI for TUI Migration

**Status:** Accepted
**Date:** 2026-02-09
**Context:** FTUI-01.1 (wa-2dlw)

## Decision

Migrate wa's interactive TUI from ratatui/crossterm to FrankenTUI (ftui).

## Context

wa's TUI (`crates/wa-core/src/tui/`) is built on ratatui 0.30.0 + crossterm 0.29.0.
It provides 7 views (Home, Panes, Events, Triage, History, Search, Help) behind a
`tui` feature flag. The implementation spans ~6,050 lines:

| Module    | Lines | Responsibility |
|-----------|-------|----------------|
| app.rs    | 2,275 | Event loop, terminal setup/teardown, key handling |
| views.rs  | 2,739 | Rendering for all 7 views |
| query.rs  |   994 | QueryClient trait abstracting data access |
| mod.rs    |    42 | Re-exports |

The codebase is well-structured with a clean QueryClient abstraction separating
data access from rendering.

The current architecture works but has structural limitations:

1. **Alt-screen only.** The TUI enters alternate screen mode on startup
   (`EnterAlternateScreen`) and leaves on exit. This destroys scrollback context
   and prevents wa from coexisting with ongoing terminal output (e.g., logs).

2. **No deterministic rendering contract.** ratatui draws via a diff-based backend
   but does not enforce a deterministic buffer/diff/present pipeline. Test assertions
   require terminal mocking, not snapshot comparison.

3. **Terminal ownership is implicit.** Nothing prevents other wa subsystems (logging,
   subprocess output) from writing to stdout while the TUI owns the terminal. This
   creates potential output corruption under concurrent use.

4. **No inline-mode path.** wa's watcher daemon logs to the same terminal session.
   Switching between TUI and daemon output requires full alt-screen transitions
   with no intermediate inline option.

FrankenTUI (`/dp/frankentui`) addresses these by design:

- **One-writer rule:** The UI runtime exclusively owns terminal writes. All other
  output is routed through a controlled sink.
- **Deterministic render pipeline:** Buffer -> diff -> present, with snapshot-testable
  intermediate states.
- **Inline-first:** Renders within scrollback by default. Alt-screen is opt-in,
  not the only mode.
- **PTY E2E harness:** Built-in testing infrastructure for lifecycle, resize,
  and input validation under real terminal conditions.

## Alternatives Considered

### 1. Incremental ratatui patching

Continue with ratatui and add custom wrappers for one-writer enforcement,
inline rendering, and deterministic testing.

**Rejected because:**
- The one-writer rule requires intercepting all stdout writes project-wide.
  Doing this as an afterthought on ratatui is error-prone and fragile.
- Inline rendering in ratatui requires fighting the library's alt-screen
  assumptions. The `Viewport::Inline` API exists but is limited and not
  the library's primary design path.
- We would duplicate work already done in ftui, creating maintenance burden
  on two parallel abstractions.

### 2. Build a custom backend

Write a custom ratatui backend that enforces one-writer and supports inline mode.

**Rejected because:**
- Still inherits ratatui's widget model and rendering assumptions.
- Custom backends diverge from upstream, making ratatui upgrades risky.
- Does not address deterministic snapshot testing without additional tooling.

### 3. No TUI (CLI-only)

Remove the TUI entirely and rely on robot mode + external dashboards.

**Rejected because:**
- The TUI provides immediate operator feedback that CLI polling cannot match.
- Human operators need real-time pane/event visibility during swarm runs.
- The TUI is already implemented and valued; removing it loses user trust.

## Consequences

### Positive

- Terminal ownership becomes explicit and enforceable.
- Tests can use deterministic snapshots instead of mocked terminals.
- Inline mode enables wa to show status without losing scrollback.
- Shared infrastructure with other `/dp/` projects reduces per-project TUI cost.

### Negative

- Migration requires touching all 4 TUI source files and their tests.
- Temporary dependency on two TUI stacks during the transition (behind feature flags).
- ftui is an internal dependency (not crates.io), requiring git-pin management.
- Risk of regression during the migration if parity is not carefully tracked.

### Neutral

- The QueryClient abstraction layer remains unchanged. Only the rendering
  and terminal management layers are affected.
- Robot mode, MCP, and all non-TUI CLI commands are unaffected.

## References

- Current TUI: `crates/wa-core/src/tui/{mod,app,views,query}.rs`
- Migration epic: wa-2wed (FTUI Adoption Program)
- Architecture contract: wa-p85q (FTUI-01)
