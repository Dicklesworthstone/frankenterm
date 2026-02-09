# Go/No-Go Cutover Review and Acceptance Pack

**Bead:** wa-1i50 (FTUI-09.4)
**Date:** 2026-02-09

---

## 1  Purpose

This document defines the evidence-based criteria for deciding whether the ratatui→ftui migration can proceed to Stage 3 (General Availability) and legacy code removal. The cutover decision must be deliberate and auditable.

## 2  Go/No-Go Checklist

### 2.1  Functional Parity

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| F1 | All 7 views render correctly (Home, Panes, Events, Triage, History, Search, Help) | `evidence/ftui-05.*/matrix.md` | Verified |
| F2 | View adapter unit tests pass (76+) | `cargo test --features ftui --lib -- view_adapters` | Verified |
| F3 | Snapshot/golden tests pass across terminal sizes (75+) | `cargo test --features ftui --lib -- snapshot_` | Verified |
| F4 | E2E headless scenarios pass (38+) | `cargo test --features ftui --lib -- e2e_` | Verified |
| F5 | TextInput edge cases pass (21+) | `cargo test --features ftui --lib -- edge_` | Verified |
| F6 | Key routing matches legacy behavior | `docs/ftui-contributor-migration-guide.md` Section 7 | Verified |
| F7 | Filter/search input works in Events, Triage, History, Search views | E2E tests + edge case matrix | Verified |

### 2.2  Infrastructure

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| I1 | Feature flags compile correctly (headless, tui, ftui) | `docs/ftui-cargo-feature-matrix.md` | Verified |
| I2 | Mutual exclusion enforced (`tui` + `ftui` = compile_error) | `wa-core/src/lib.rs:129` | Verified |
| I3 | Import guardrails prevent ratatui reintroduction | `wa-core/src/tui/mod.rs` guardrail tests | Verified |
| I4 | CI gates configured (fmt, clippy, tests, snapshots) | `.github/workflows/` | Verified |
| I5 | Terminal session lifecycle tested (50+) | `cargo test --features tui --lib -- terminal_session` | Verified |
| I6 | Command handoff state machine tested (19+) | `cargo test --features tui --lib -- command_handoff` | Verified |
| I7 | Output gate tested (21+) | `cargo test --features tui --lib -- output_gate` | Verified |

### 2.3  Performance

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| P1 | Pre/post migration baselines captured | `evidence/ftui-08.1/perf-baselines.md` | Verified |
| P2 | No S1/S2 performance regressions | wa-avl6 hot path optimization | Blocked on P1 |
| P3 | Rendering latency within 60fps budget (<16ms p95) | Worst case: 86µs (0.52% of budget) | Verified |

### 2.4  Compatibility

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| C1 | WezTerm (Linux) headless certified | `evidence/ftui-08.3/compatibility-matrix.md` | CONDITIONAL PASS |
| C2 | WezTerm (macOS) certified | Per-environment runbook | Not started |
| C3 | tmux (Linux) certified | Per-environment runbook | Not started |
| C4 | At least 3 P0+P1 environments fully certified | Compatibility matrix | 1/3 |
| C5 | Known limitations documented with mitigations | Compatibility matrix Section 2.5 | Verified |

### 2.5  Resilience

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| R1 | Chaos/resilience tests pass (20+) | `evidence/ftui-08.4/stability-report.md` | Verified |
| R2 | Zero-dimension guard documented | Stability report finding #1 | Verified |
| R3 | Panic handler restores terminal | `docs/ftui-teardown-harness.md` | Verified |
| R4 | Subprocess handoff robust under failure | `docs/ftui-command-handoff-traces.md` | Verified |

### 2.6  Documentation

| # | Criterion | Evidence Artifact | Status |
|---|-----------|------------------|--------|
| D1 | Contributor migration guide published | `docs/ftui-contributor-migration-guide.md` | Verified |
| D2 | Operator behavioral changes documented | Migration guide Section 7 | Verified |
| D3 | Rollout strategy with stages defined | `docs/ftui-rollout-strategy.md` | Verified |
| D4 | Decommission plan with removal sequence | `docs/ftui-decommission-plan.md` | Verified |
| D5 | Compatibility runbook template ready | `docs/ftui-compat-runbook-template.md` | Verified |

## 3  Decision Matrix

| Verdict | Criteria |
|---------|----------|
| **GO** | All F, I, P, C, R, D criteria met. Zero open S1/S2 issues. Stage 2 beta ran >= 4 weeks. |
| **CONDITIONAL GO** | All F, I, R, D criteria met. P and C have documented plans with timelines. No S1 issues. |
| **NO-GO** | Any F or I criterion not met, OR any open S1 issue, OR Stage 2 beta < 4 weeks. |

## 4  Current Assessment

### Summary (2026-02-09)

| Category | Status | Blockers |
|----------|--------|----------|
| Functional (F1-F7) | All met | None |
| Infrastructure (I1-I7) | All met | None |
| Performance (P1-P3) | Partial | P1+P3 met; P2 blocked on wa-avl6 |
| Compatibility (C1-C5) | Partial | PTY E2E + macOS/tmux certification needed |
| Resilience (R1-R4) | All met | None |
| Documentation (D1-D5) | All met | None |

**Current verdict: CONDITIONAL GO** (Performance baselines captured; P2 regression analysis + compatibility certification remaining)

### Remaining Work for GO

1. Complete wa-avl6 (hot path optimization / regression analysis) — P2
2. Certify at least 2 more P0+P1 environments (WezTerm macOS, tmux Linux)
4. Run Stage 1 canary for >= 2 weeks
5. Run Stage 2 beta for >= 4 weeks

## 5  Sign-Off Roles

| Role | Responsibility | Required For |
|------|---------------|-------------|
| **Migration Lead** | Validates all F, I, D criteria | GO and CONDITIONAL GO |
| **Performance Reviewer** | Validates P criteria, approves regressions | GO |
| **Compatibility Reviewer** | Validates C criteria, approves known limitations | GO |
| **Project Owner** | Final GO/NO-GO decision | All |

## 6  Cutover Readiness Report Template

```markdown
# Cutover Readiness Report

**Date:** YYYY-MM-DD
**Reviewer:** [name]

## Checklist Status
- Functional: [X/7] met
- Infrastructure: [X/7] met
- Performance: [X/3] met
- Compatibility: [X/5] met
- Resilience: [X/4] met
- Documentation: [X/5] met

## Open Issues
| ID | Severity | Description | Owner | ETA |
|----|----------|-------------|-------|-----|

## Verdict
[ ] GO — proceed to Stage 3
[ ] CONDITIONAL GO — proceed with conditions: [list]
[ ] NO-GO — blockers: [list]

## Sign-Offs
| Role | Name | Date | Decision |
|------|------|------|----------|
| Migration Lead | | | |
| Performance Reviewer | | | |
| Compatibility Reviewer | | | |
| Project Owner | | | |
```

## 7  References

- `docs/ftui-rollout-strategy.md` — Stage definitions and transitions
- `docs/ftui-decommission-plan.md` — Post-cutover removal sequence
- `docs/ftui-contributor-migration-guide.md` — Migration guide
- `evidence/ftui-08.3/compatibility-matrix.md` — Compatibility matrix
- `evidence/ftui-08.4/stability-report.md` — Resilience validation
