# Post-Cutover Stabilization and Bug-Bash Program

**Bead:** wa-1b2n (FTUI-09.5)
**Date:** 2026-02-09

---

## 1  Stabilization Window

### Duration and Phases

| Phase | Duration | Focus | Exit Criteria |
|-------|----------|-------|---------------|
| **Hypercare** | Week 1-2 | S1/S2 triage, rapid hotfixes | Zero open S1 issues |
| **Stabilization** | Week 3-4 | S3 triage, edge case fixes, doc updates | S3 backlog < 5 items |
| **Steady state** | Week 5+ | Normal development cadence | No stabilization-specific process |

### Hypercare Protocol

During weeks 1-2 after cutover to Stage 3:

1. **Daily triage** — Review all new TUI-related issue reports
2. **Hotfix SLA** — S1 issues: fix within 4 hours, deploy same day
3. **Rollback readiness** — Stage 2 binary kept available for emergency revert
4. **Monitoring** — Check panic hook logs, terminal restoration failures

### Escalation Path

| Severity | Response Time | Owner | Action |
|----------|--------------|-------|--------|
| S1 (crash/data loss) | 4 hours | Migration Lead | Hotfix + emergency release |
| S2 (terminal corruption) | 1 business day | Migration Lead | Hotfix + next release |
| S3 (rendering glitch) | 1 week | Assigned dev | Normal PR flow |
| S4 (cosmetic) | Backlog | Assigned dev | Normal PR flow |

## 2  Bug-Bash Protocol

### Scheduled Sessions

Run 2 bug-bash sessions during the stabilization window:

| Session | Timing | Scope | Participants |
|---------|--------|-------|-------------|
| Bug-bash 1 | End of Week 1 | Core workflows: view navigation, filter input, key bindings | All contributors |
| Bug-bash 2 | End of Week 3 | Edge cases: multiplexer stacks, extreme sizes, rapid input | Targeted testers |

### Bug-Bash Procedure

1. **Setup**: Each participant runs `wa tui` in their daily terminal environment
2. **Checklist execution**: Work through the compatibility runbook manual checks (9 items)
3. **Exploratory testing**: 30 minutes of unscripted usage focusing on their workflows
4. **Report**: File issues with `ftui-stabilization` label, include:
   - Environment metadata (terminal, OS, $TERM, multiplexer)
   - Steps to reproduce
   - Screenshot or terminal recording
   - Severity assessment (S1-S4)

### Issue Template

```markdown
**Environment:** [terminal] [OS] [multiplexer] [$TERM]
**wa version:** [commit hash]
**Severity:** S[1-4]

**Steps to reproduce:**
1. ...

**Expected:** ...
**Actual:** ...

**Screenshot/recording:** [attached]
```

## 3  Ownership Routing

| Issue Category | Primary Owner | Backup |
|---------------|--------------|--------|
| Rendering / layout | ftui_stub.rs owner | view_adapters.rs owner |
| Key routing / input | keymap.rs owner | ftui_stub.rs owner |
| Terminal lifecycle | terminal_session.rs owner | crash.rs owner |
| Performance | Perf reviewer | Migration Lead |
| Compatibility | Compat reviewer | Migration Lead |
| Documentation | Migration Lead | Any contributor |

## 4  Follow-Up Bead Generation

### Triage Workflow

```
Bug report → Severity classification → Bead creation → Assignment → Fix → Verify → Close
```

### Bead Template for Stabilization Issues

```
Title: FTUI-STAB-NNN: [short description]
Type: task
Priority: P[0-3] based on severity
Labels: ftui, stabilization
Parent: wa-1b2n (FTUI-09.5)
```

### Severity → Priority Mapping

| Severity | Priority | Bead Labels |
|----------|----------|-------------|
| S1 (crash) | P0 | `ftui, stabilization, s1` |
| S2 (corruption) | P1 | `ftui, stabilization, s2` |
| S3 (glitch) | P2 | `ftui, stabilization` |
| S4 (cosmetic) | P3 | `ftui, stabilization` |

## 5  Success Metrics

The stabilization window is considered successful when:

- [ ] Zero open S1 issues for 2 consecutive weeks
- [ ] S2 backlog < 3 items, all with documented workarounds
- [ ] S3 backlog < 5 items
- [ ] Bug-bash sessions completed with findings triaged
- [ ] No `WA_TUI_BACKEND=ratatui` override requests from operators
- [ ] Decommission plan (FTUI-09.3) ready for execution

## 6  Documentation Updates

During stabilization, update these documents with real-world findings:

| Document | Update Type |
|----------|------------|
| `docs/ftui-contributor-migration-guide.md` | Add troubleshooting entries from bug reports |
| `evidence/ftui-08.3/compatibility-matrix.md` | Add newly certified environments |
| `docs/ftui-compat-runbook-template.md` | Refine based on bug-bash feedback |
| `CHANGELOG.md` | Document behavioral changes and fixes |

## 7  References

- `docs/ftui-go-nogo-checklist.md` — Cutover review criteria
- `docs/ftui-rollout-strategy.md` — Rollout stages
- `docs/ftui-decommission-plan.md` — Post-stabilization removal plan
- `docs/ftui-compat-runbook-template.md` — Per-environment testing
