# ADR-0003: Migration Scope, Constraints, and Tradeoffs

**Status:** Accepted
**Date:** 2026-02-09
**Context:** FTUI-01.1 (wa-2dlw)

## Decision

Define explicit scope boundaries, constraints, and accepted tradeoffs for the
ftui migration to prevent scope creep and enable parallel execution.

## Scope

### In scope

1. **Rendering layer replacement.** Replace ratatui widget rendering with ftui
   equivalents in all 7 views (Home, Panes, Events, Triage, History, Search, Help).

2. **Terminal management migration.** Replace crossterm terminal setup/teardown
   (`enable_raw_mode`, `EnterAlternateScreen`, etc.) with ftui's terminal
   ownership model.

3. **Event loop migration.** Replace the crossterm event polling loop with ftui's
   event handling.

4. **Feature flag coexistence.** During migration, both `tui` (legacy ratatui)
   and `ftui` (new) feature flags exist. The legacy flag is removed only after
   parity is confirmed.

5. **Test migration.** Replace any ratatui-specific test assertions with ftui
   snapshot tests and PTY E2E scenarios.

6. **Input/keybinding parity.** All current keybindings (global: q/?/r/Tab/1-7,
   per-view: j/k/arrows/Enter/Esc/f/b) must work identically after migration.

### Out of scope

1. **Non-TUI code.** Robot mode, MCP server, storage layer, pattern engine,
   watcher daemon, workflow engine, policy engine. These are unaffected.

2. **New TUI features.** The migration delivers parity, not new functionality.
   New views, widgets, or interactions are separate beads.

3. **CLI output formatting.** Human CLI output (tables, panels via `rich_rust`)
   is independent of the TUI stack.

4. **Architecture changes to QueryClient.** The data access abstraction is
   unchanged. Only the consumers (views) change their rendering implementation.

## Constraints

### C1: Parity before progress

No new TUI features may land until the ftui migration reaches view-level parity
with the ratatui implementation. This prevents divergence between two incomplete
stacks.

### C2: Feature flag isolation

All ftui code must be behind `#[cfg(feature = "ftui")]`. The project must build
and pass tests with either `--features tui` (legacy) or `--features ftui` (new)
but not both simultaneously. This prevents cross-contamination.

### C3: No ratatui API leakage into wa-core public interface

ratatui types (Widget, Buffer, Rect, etc.) must not appear in wa-core's public
API outside the `tui` module. This is already true today and must remain true
during migration. Similarly, ftui types must not leak.

### C4: Evidence-driven acceptance

Every migrated view must produce three evidence types:
- **Unit tests** for adapter/reducer logic.
- **Snapshot tests** for rendered output at multiple terminal sizes.
  Snapshots must be hermetic (no system clock, no network, no real WezTerm).
- **PTY E2E test** proving the view works in a real terminal.

No migrated view is considered done without all three evidence types.

Any intentional deviation from current behavior must be documented in the parity
ledger with: what changed, why, user-impact assessment, and rollback consideration.

### C5: Build time budget

Adding ftui as a dependency must not increase clean-build time by more than 30
seconds on CI. If it does, investigate feature gating or dependency trimming.

## Tradeoffs

### T1: Temporary dual-stack complexity

**Accepted.** During migration, two TUI stacks coexist behind feature flags.
This adds conditional compilation complexity and doubles TUI-related CI matrix
entries. The alternative (big-bang replacement) risks extended breakage.

**Mitigation:** Track dual-stack duration in the migration timeline. Target
complete removal of ratatui within 2 sprints of ftui reaching parity.

### T2: ftui is an internal dependency

**Accepted.** ftui is not on crates.io. We depend on it via git reference
(path or rev pin). This means:
- Version updates require explicit pin bumps.
- Breaking upstream changes require coordination.

**Mitigation:** Pin to a specific git rev (not branch). Define sync policy
in FTUI-02.2 (wa-e2jh).

### T3: Migration effort is front-loaded

**Accepted.** The first 3 phases (architecture, dependency, runtime) produce
no user-visible improvement. Users see benefit only when views start migrating
(phase 5+). This requires patience and clear communication.

**Mitigation:** The architecture and dependency phases are small (ADRs, Cargo
config, ownership abstraction). View migration is incremental and delivers
visible progress per-view.

### T4: Inline mode is new behavior

**Accepted.** The current TUI is alt-screen only. ftui's inline-first model
introduces new behavior that may surprise users who expect full-screen mode.

**Mitigation:** Default to alt-screen for initial migration (behavior parity).
Inline mode is additive and opt-in. Document the mode switch in FTUI-03.3
(wa-21cz).

### T5: View rendering may differ in edge cases

**Accepted.** ftui widgets are not pixel-identical to ratatui widgets.
Minor rendering differences (spacing, border styles, color handling) are
expected and acceptable if:
- Information content is preserved.
- Keybinding behavior is identical.
- Layout structure is equivalent (same regions, same content placement).

**Mitigation:** The parity contract (FTUI-01.3, wa-136q) defines what "parity"
means. Intentional deltas are logged in the parity ledger with rationale.

## Non-goals (explicit)

- Rewrite the TUI from scratch. This is a migration, not a redesign.
- Change the view set. The same 7 views exist post-migration.
- Alter data flow. QueryClient -> ViewState -> Render remains the pattern.
- Optimize rendering performance. Performance is tracked (FTUI-08) but is
  not a migration blocker unless it regresses catastrophically.

## References

- Current TUI source: `crates/wa-core/src/tui/`
- Current views: Home, Panes, Events, Triage, History, Search, Help
- Current keybindings: `app.rs:170-237` (global), per-view handlers following
- Parity contract bead: wa-136q (FTUI-01.3)
- Dependency integration bead: wa-1utb (FTUI-02.1)
