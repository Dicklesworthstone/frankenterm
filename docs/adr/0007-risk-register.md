# Risk Register: FTUI Migration

**Bead:** wa-co0h (FTUI-01.4)
**Date:** 2026-02-09

## Risk Register

| ID | Risk | Likelihood | Impact | Owner | Trigger | Mitigation |
|----|------|-----------|--------|-------|---------|------------|
| R1 | Terminal left in raw mode after panic | Medium | High | FTUI-03 | Any panic during TUI operation | Panic hook calls terminal restore. See ADR-0002 R5. |
| R2 | Rendering regression (missing content) | Medium | High | FTUI-05 | Snapshot test diff shows missing data | Per-view rollback via feature flag. Fix view before re-enabling. |
| R3 | Keybinding loss (dropped input) | Low | Critical | FTUI-06 | Any keybinding from parity contract stops working | Immediate rollback. Input matrix tested in PTY E2E. |
| R4 | Build time regression | Low | Medium | FTUI-02 | Clean build >30s slower than baseline | Trim ftui dependencies or split feature flags further. |
| R5 | ftui upstream breaking change | Medium | Medium | FTUI-02 | `cargo check --features ftui` fails after pin bump | Freeze pin until fix lands. Compatibility adapter absorbs change. |
| R6 | Performance regression (render latency) | Low | Medium | FTUI-08 | Frame render time >2x ratatui baseline | Profile and optimize hot paths. Defer ftui default until resolved. |
| R7 | Dual-stack drift during migration | Medium | Low | FTUI-02 | Legacy `tui` and new `ftui` diverge in behavior | CI tests both flags. Migration timeline keeps dual-stack window short. |
| R8 | Inline mode confuses users | Low | Low | FTUI-03 | User feedback reports unexpected behavior | Default to alt-screen. Inline is opt-in only. |
| R9 | Log output corrupts TUI display | Medium | Medium | FTUI-03 | Visible garbage during TUI operation | One-writer enforcement catches violations. Route logs to sink. |
| R10 | Command handoff leaves terminal inconsistent | Low | High | FTUI-06 | Terminal state wrong after running external command | Structured handoff protocol with full redraw on resume. |

## Rollback Procedures

### Feature-flag rollback (any phase)

```bash
# Build with legacy TUI instead of ftui
cargo build --release --features tui
# (omit --features ftui)
```

No code changes required. The legacy `tui` feature compiles the ratatui-based
implementation. This is the primary rollback mechanism for all migration phases.

### Per-view rollback (FTUI-05)

During view migration, each view is migrated independently. If a single view
fails parity:

1. Revert the view's ftui implementation.
2. The view falls back to the ratatui version under `--features tui`.
3. Other migrated views continue under `--features ftui`.

This requires the dual-stack to remain compilable until all views are migrated.

### Full migration rollback (FTUI-09)

If the go/no-go review at Phase 9 fails:

1. Re-add `tui` feature flag and ratatui/crossterm dependencies.
2. Restore ratatui view implementations from git history.
3. Remove `ftui` as default. Ship with `--features tui`.

This is expensive but possible because the QueryClient layer is unchanged.

## Trigger Thresholds

| Metric | Acceptable | Warning | Rollback |
|--------|-----------|---------|----------|
| Snapshot test failures | 0 | 1 (with documented delta) | 2+ unexplained |
| Keybinding failures | 0 | 0 | 1+ |
| PTY E2E failures | 0 | 1 (flaky, with retry) | 2+ stable failures |
| Build time delta | <10s | 10-30s | >30s |
| Render frame time | <baseline * 1.2 | <baseline * 1.5 | >baseline * 2.0 |
| Terminal corruption reports | 0 | 0 | 1+ |

## References

- ADR-0004: Phased Rollout and Rollback Strategy
- ADR-0003: Migration Scope, Constraints, Tradeoffs
- Parity contract: ADR-0006
- Go/no-go review: wa-1i50 (FTUI-09.4)
