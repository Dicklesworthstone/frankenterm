# ADR-0009: Upstream Sync and Pinning Policy for /dp/frankentui

**Status:** Accepted
**Date:** 2026-02-09
**Bead:** wa-e2jh (FTUI-02.2)
**Epic:** wa-1k52 (FTUI-02)

## Context

wa consumes `/dp/frankentui` (ftui) as an optional dependency during the TUI
migration. The dependency is currently a path dep for fast iteration
(see ADR-004). Before any release, the dependency must be pinned to a
specific upstream commit to guarantee reproducible builds.

This document defines when and how we pin, how we review upstream changes,
and what compatibility risks to watch for.

## Decision

### Dependency Phases

| Phase | Cargo Dependency | When |
|-------|-----------------|------|
| **Development** | `path = "../frankentui/crates/ftui"` | Default during migration work |
| **Pre-release** | `git = "...", rev = "<sha>"` | Before tagging any wa release |
| **Post-migration** | `version = "x.y"` (crates.io) | After ftui is published and stable |

### Pin Cadence

- **During migration**: Pin to a specific commit at least weekly, or when
  a breaking API change lands in ftui.
- **Before wa release**: Pin is mandatory. The exact rev is recorded in the
  root `Cargo.toml` comment (already present: `Pinned rev for reference: 65b8538`).
- **Routine bumps**: Bump the pin when ftui adds features wa needs, or when
  security/correctness fixes land upstream.

### Pin Procedure

1. Fetch the latest ftui main:
   ```bash
   cd /dp/frankentui && git pull origin main
   ```
2. Record the new HEAD:
   ```bash
   REV=$(cd /dp/frankentui && git rev-parse --short HEAD)
   echo "New pin: $REV"
   ```
3. Update the root `Cargo.toml`:
   ```toml
   # Pinned rev for reference: <new-rev> (YYYY-MM-DD)
   ftui = { path = "../frankentui/crates/ftui", default-features = false, features = ["runtime"] }
   ```
   When switching to git dep for release:
   ```toml
   ftui = { git = "https://github.com/.../frankentui", rev = "<new-rev>", default-features = false, features = ["runtime"] }
   ```
4. Run the guardrail checks:
   ```bash
   scripts/check_ftui_guardrails.sh
   ```
5. Run the full test suite:
   ```bash
   cargo test -p wa-core --features ftui
   ```
6. Commit with message: `chore(deps): bump ftui pin to <rev>`

### Change-Review Checklist

When bumping the ftui pin, review the upstream diff for:

- [ ] **Breaking API changes** in types wa uses (`Model`, `Cmd`, `Frame`,
      `Buffer`, `Event`, `KeyCode`, `Style`, `App`, `ScreenMode`)
- [ ] **Feature flag changes** — verify `runtime` feature still exports what
      wa needs
- [ ] **Dependency additions** — check for new transitive deps that might
      conflict with wa's dependency graph (especially `unicode-width`,
      `crossterm`, or async runtime crates)
- [ ] **Minimum Rust version** — ftui must not require a newer MSRV than wa
      (currently nightly/1.85)
- [ ] **Removed re-exports** — ensure nothing wa imports was dropped from
      `ftui/src/lib.rs`

### Compatibility Risk Checklist

| Risk | Mitigation |
|------|-----------|
| ftui `Model` trait changes signature | `ftui_compat.rs` isolates the boundary; update adapter first |
| ftui `Frame`/`Buffer` API changes | `RenderSurface` trait absorbs the change |
| ftui removes `runtime` feature | Pin prevents surprise; review before bumping |
| ftui adds conflicting transitive dep | Path dep during dev catches this early; fix before pinning |
| ftui changes `KeyCode` variants | `ftui_compat::Key` enum absorbs the mapping |

### Automation

- The `check_ftui_guardrails.sh` script (wa-eutd) runs in CI and catches
  compilation failures from API drift after a pin bump.
- The feature matrix CI job tests `--features ftui` independently on every PR.
- Future: Add a scheduled CI job that checks compatibility with ftui HEAD
  (canary build, non-blocking).

## Consequences

- Path dep during development allows rapid iteration without version ceremony.
- Git pin before release guarantees reproducible builds.
- The review checklist prevents surprise breakage from upstream changes.
- The adapter layer (ftui_compat) absorbs most API drift, limiting the
  blast radius of upstream changes to a single module.

## References

- ADR-004: FTUI Dependency Strategy
- wa-eutd: Build guardrails (check_ftui_guardrails.sh)
- wa-1utb: Dependency integration
