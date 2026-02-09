# Phased Rollout / Canary Strategy: ratatui → FrankenTUI

**Bead:** wa-wzft (FTUI-09.2)
**Date:** 2026-02-09

---

## 1  Rollout Stages

### Stage 0: Development (current)

| Property | Value |
|----------|-------|
| **Feature default** | `tui` (legacy) |
| **ftui access** | `--features ftui` opt-in |
| **Audience** | Contributors, CI |
| **Duration** | Until all FTUI-08 acceptance criteria met |
| **Exit criteria** | 320+ headless tests pass, compatibility matrix complete, no S1/S2 issues |

### Stage 1: Canary

| Property | Value |
|----------|-------|
| **Feature default** | `tui` (legacy) |
| **ftui access** | `WA_TUI_BACKEND=ftui` environment variable |
| **Audience** | Internal operators, power users |
| **Duration** | 2 weeks minimum |
| **Entry criteria** | Stage 0 exit criteria met |
| **Exit criteria** | Zero S1/S2 reports from canary users, PTY E2E pass on P0+P1 environments |

**Canary activation:**
```bash
# Operator enables ftui for their session
export WA_TUI_BACKEND=ftui
wa tui
```

**Fallback:**
```bash
# Revert to legacy
unset WA_TUI_BACKEND
wa tui
```

### Stage 2: Beta

| Property | Value |
|----------|-------|
| **Feature default** | `ftui` (new default) |
| **Legacy access** | `WA_TUI_BACKEND=ratatui` override |
| **Audience** | All users |
| **Duration** | 4 weeks minimum |
| **Entry criteria** | Stage 1 exit criteria met |
| **Exit criteria** | Zero S1/S2 for 4 consecutive weeks, no operator override reports |

**Override to legacy (escape hatch):**
```bash
export WA_TUI_BACKEND=ratatui
wa tui
```

### Stage 3: General Availability

| Property | Value |
|----------|-------|
| **Feature default** | `ftui` only |
| **Legacy access** | Removed (ratatui code deleted) |
| **Audience** | All users |
| **Entry criteria** | Stage 2 exit criteria met, decommission plan executed |

## 2  Feature Flag Implementation

### Runtime Backend Selection (implemented)

The `WA_TUI_BACKEND` environment variable controls which backend is used at
runtime.  Build with `--features rollout` to compile both backends into a single
binary.

**Implementation:** `crates/wa-core/src/tui/rollout.rs`

```rust
// select_backend() reads WA_TUI_BACKEND and returns the active backend.
// Recognized values: "ftui", "frankentui", "ratatui", "legacy".
// Unrecognized or unset → stage default (currently Ratatui for Stage 1).
pub fn select_backend() -> TuiBackend { ... }

// run_tui() dispatches to the selected backend's run_tui().
pub fn run_tui<Q: QueryClient + Send + Sync + 'static>(...) { ... }
```

### Compile-Time Feature Matrix by Stage

| Stage | Cargo features | Binary size impact |
|-------|---------------|-------------------|
| 0 (Dev) | `tui` OR `ftui` (exclusive) | Single backend |
| 1 (Canary) | `rollout` (compiles both via `tui` + `ftui`) | ~15-20% increase |
| 2 (Beta) | `rollout` (both compiled, default swapped to ftui) | Same as Stage 1 |
| 3 (GA) | `ftui` only | Returns to single backend |

### Stage Transition Checklist

Each stage transition requires:

- [ ] All stage exit criteria met (documented in evidence)
- [ ] No open S1/S2 issues in the issue tracker
- [ ] Operator communication sent (changelog, migration notes)
- [ ] CI gates updated for new default
- [ ] Rollback procedure tested

## 3  Rollback Triggers

### Automatic Rollback Criteria

| Trigger | Severity | Action |
|---------|----------|--------|
| Panic in ftui rendering | S1 | Revert to previous stage |
| Terminal not restored after exit | S2 | Revert to previous stage |
| Data loss in TUI interaction | S1 | Revert to previous stage |
| >3 operator-reported rendering issues in 1 week | S3 aggregate | Hold stage, investigate |

### Rollback Procedure

**Stage 1 → Stage 0:**
```bash
# Remove canary environment variable
unset WA_TUI_BACKEND
# Rebuild with legacy-only features
cargo build -p wa --features tui
```

**Stage 2 → Stage 1:**
```bash
# Change default back to legacy
# Update default_for_stage() to return TuiBackend::Ratatui
# Rebuild and deploy
```

**Stage 3 → Stage 2:**
Not possible — ratatui code is deleted. This is why Stage 2 must run for a minimum of 4 weeks with zero S1/S2 issues.

## 4  Monitoring and Metrics

### Key Metrics per Stage

| Metric | Collection Method | Threshold |
|--------|------------------|-----------|
| Panic rate | Panic hook reporting | 0 panics |
| Terminal restoration failures | SessionGuard Drop logging | 0 failures |
| Rendering latency (p95) | Tick timing in debug logs | <16ms (60fps budget) |
| User-reported issues | Issue tracker | <3 per week for stage advancement |
| Test pass rate | CI | 100% for all headless suites |

### Evidence Collection

Each stage transition must produce:
1. Test results snapshot (headless test counts + pass rates)
2. Canary/beta user feedback summary
3. Issue tracker scan (S1/S2 count = 0)
4. Performance comparison (pre/post migration baselines from wa-290k)
5. Compatibility matrix status (from wa-e69a)

## 5  Communication Plan

### Stage 1 Entry
- CHANGELOG entry: "ftui canary available via `WA_TUI_BACKEND=ftui`"
- Migration guide link: `docs/ftui-contributor-migration-guide.md`
- Known limitations from compatibility matrix

### Stage 2 Entry
- CHANGELOG entry: "ftui is now the default TUI backend"
- Legacy escape hatch instructions
- Behavioral changes summary (from migration guide Section 7)

### Stage 3 Entry
- CHANGELOG entry: "ratatui/crossterm dependencies removed"
- Breaking change notice for any custom ratatui integrations
- Decommission report reference

## 6  Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ftui rendering regression on untested terminal | Medium | S2 | Compatibility matrix certification + runbook |
| Performance regression vs ratatui | Low | S3 | Pre/post baselines (wa-290k) + hot path optimization (wa-avl6) |
| Key binding behavioral change | Low | S3 | Configurable keymap + defaults matching legacy |
| Panic in production during Stage 1 | Low | S1 | Panic hook restores terminal; canary is opt-in |
| Stage 2 default change breaks automation | Low | S2 | `WA_TUI_BACKEND=ratatui` escape hatch |
| Stage 3 removes escape hatch too early | Medium | S2 | 4-week minimum beta period; rollback not possible |

## 7  References

- `docs/ftui-cargo-feature-matrix.md` — Feature flag matrix
- `docs/ftui-contributor-migration-guide.md` — Contributor migration guide
- `docs/ftui-compat-runbook-template.md` — Per-environment certification template
- `evidence/ftui-08.3/compatibility-matrix.md` — Compatibility matrix
- `evidence/ftui-08.4/stability-report.md` — Chaos validation report
