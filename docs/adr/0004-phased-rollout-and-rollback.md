# ADR-0004: Phased Rollout and Rollback Strategy

**Status:** Accepted
**Date:** 2026-02-09
**Context:** FTUI-01.1 (wa-2dlw)

## Decision

The ftui migration follows a phased rollout with explicit rollback triggers
at each phase boundary. Feature flags enable instant rollback without code
changes.

## Phases

### Phase 1: Architecture Contract (FTUI-01)

**Deliverables:** ADR set, ring map, parity contract.
**Rollback:** N/A (documentation only, no code changes).
**Gate:** All ADRs reviewed and committed.

### Phase 2: Dependency Integration (FTUI-02)

**Deliverables:** ftui in Cargo.toml behind `ftui` feature flag, build guards.
**Rollback:** Remove `ftui` feature from Cargo.toml. Single commit revert.
**Gate:** `cargo check --features ftui` passes. `cargo check --features tui` still
passes. Build time within budget (C5).

### Phase 3: Runtime Ownership (FTUI-03)

**Deliverables:** Terminal ownership abstraction, one-writer enforcement, log routing.
**Rollback:** The ownership abstraction is behind `ftui` flag. Revert to direct
crossterm calls under `tui` flag.
**Gate:** Ownership model passes unit tests. No output corruption under concurrent
log/render scenarios.

### Phase 4: Query-to-View Adapter (FTUI-04)

**Deliverables:** Adapter layer converting QueryClient data to ftui view models.
**Rollback:** Remove adapter code. Views continue using ratatui types under `tui`.
**Gate:** Adapter tests pass with fixture data.

### Phase 5: View Migration (FTUI-05)

**Deliverables:** Each view migrated individually: Home, Panes, Events, Triage,
History, Search, Help.
**Rollback:** Per-view. Each view has its own ftui implementation gated on
`#[cfg(feature = "ftui")]`. Reverting a single view means it falls back to the
ratatui implementation under `tui`.
**Gate per view:**
- Snapshot test matches expected output.
- Keybinding behavior matches legacy (tested via input simulation).
- No panic under empty data, single item, and large dataset.
- Parity evidence logged in the parity ledger.

### Phase 6: Input and Interaction (FTUI-06)

**Deliverables:** Keybinding parity, modal interactions, filter widgets, command
handoff.
**Rollback:** Same feature-flag mechanism as Phase 5.
**Gate:** Full keybinding matrix tested. Command handoff state machine has
trace evidence.

### Phase 7: Testing and CI (FTUI-07)

**Deliverables:** Unit test matrix, snapshot suite, PTY E2E scenarios, CI gates.
**Rollback:** N/A (tests are additive).
**Gate:** CI pipeline passes with `--features ftui`. All PTY E2E scenarios pass.

### Phase 8: Hardening (FTUI-08)

**Deliverables:** Performance baselines, compatibility matrix, resilience testing.
**Rollback:** N/A (observability only).
**Gate:** No performance regression >20% vs ratatui baseline. Terminal
compatibility covers at least: WezTerm, Ghostty, tmux.

### Phase 9: Rollout and Decommission (FTUI-09)

**Deliverables:** Default to ftui. Remove ratatui. Update docs.
**Rollback:** Re-add ratatui feature flag if critical issues found post-release.
**Gate:** Go/no-go review confirms all parity evidence, performance baselines,
and compatibility results.

## Rollback Triggers

Rollback is triggered if any of the following occur:

1. **Rendering regression** visible in snapshot tests that cannot be explained
   as an intentional delta.
2. **Input loss** where keystrokes are dropped or misrouted.
3. **Panic** in any TUI code path under normal operation.
4. **Build time regression** exceeding 60 seconds (2x the budget).
5. **Performance regression** exceeding 50% in render frame time or event
   handling latency.

## Feature Flag Design

```toml
# Cargo.toml (workspace level)
[features]
tui = ["dep:ratatui", "dep:crossterm"]     # Legacy (current)
ftui = ["dep:frankentui"]                   # New (migration target)
```

Rules:
- `tui` and `ftui` are mutually exclusive. Compiling with both is a build error
  (enforced by a `compile_error!` guard).
- During migration, CI tests both `--features tui` and `--features ftui`.
- Post-migration, `tui` is removed entirely.

## Canary Strategy

Before making ftui the default:

1. Run ftui in CI for 1 week with all tests green.
2. Developers use `--features ftui` locally for daily work.
3. Document any delta in the parity ledger.
4. Go/no-go review at Phase 9 gate.

## References

- Migration epic: wa-2wed
- Risk register bead: wa-co0h (FTUI-01.4)
- Build guards bead: wa-eutd (FTUI-02.4)
- Decommission plan bead: wa-1q7m (FTUI-09.3)
- Go/no-go review bead: wa-1i50 (FTUI-09.4)
