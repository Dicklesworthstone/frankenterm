# Resize Incident Response and Rollback Runbook (`wa-1u90p.8.4`)

Date: 2026-02-15  
Status: Draft operator runbook for resize/reflow incidents and rollback execution  
Depends on: `wa-1u90p.8.1`, `wa-1u90p.8.2`, `wa-1u90p.7.4`, `wa-1u90p.7.5`

## Purpose

Provide a deterministic operator playbook for responding to resize/reflow regressions:

1. Classify incident severity quickly.
2. Contain blast radius with explicit rollout controls.
3. Execute rollback safely with evidence capture.
4. Verify fallback posture before resuming rollout.

Read with:
- `docs/resize-rollout-plan-wa-1u90p.8.2.md`
- `docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md`
- `docs/resize-performance-slos.md`
- `docs/resize-artifact-fault-model-wa-1u90p.4.1.md`

## Scope and Triggers

This runbook applies when any resize/reflow trigger fires:

1. Compatibility invariant breach (`RC-ALTSCREEN-001`, `RC-INTERACTION-001`, `RC-LIFECYCLE-001`).
2. Critical visual artifact observed (`A1`/`A2`/`A3`/`A4` classes from fault model).
3. `M1`/`M2` SLO breach sustained across two gate runs.
4. Crash loop, hang, or watchdog emergency safe-mode activation during resize windows.
5. Lock/memory pressure escalation during resize storms that does not recover within response window.

## Severity Matrix

| Severity | Entry condition | Immediate posture |
|---|---|---|
| `SEV-1` | Invariant breach, crash loop, or emergency safe-mode | Immediate rollback to legacy path and rollout freeze |
| `SEV-2` | Repeated artifact or major p99 latency breach without invariant failure | Halt cohort expansion, reduce exposure, start rollback decision clock |
| `SEV-3` | Warning-level drift (queueing/latency/pressure) without user-visible breakage | Hold rollout progression, capture telemetry, remediate before next phase |

## Ownership and SLAs

| Condition | Incident commander | Required approver | Response SLA |
|---|---|---|---|
| `SEV-1` | Resize on-call owner | Rollout approver | 5 minutes |
| `SEV-2` | Resize on-call owner | Rollout approver | 15 minutes |
| `SEV-3` | Resize on-call owner | None (notify approver) | 30 minutes |

## 15-Minute Triage Loop

Run this loop before deeper diagnosis:

```bash
ft status --health
ft robot events --limit 50
ft robot search "resize OR reflow OR artifact OR watchdog OR emergency_disable" --limit 100
```

If reproducing from deterministic baselines:

```bash
ft simulate run fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml --json --resize-timeline-json
ft simulate run fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml --json --resize-timeline-json
```

## Response Procedure

### Step 1: Contain

1. Freeze rollout progression (`GO` decisions suspended).
2. Record current cohort (`C0`/`C1`/`C2`/`C3`) and active feature posture.
3. If `SEV-1`, apply emergency rollback posture immediately:
   - `ResizeSchedulerConfig.emergency_disable=true`
   - `ResizeSchedulerConfig.legacy_fallback_enabled=true`
4. If `SEV-2`, reduce exposure to previous stable cohort while evidence is gathered.

### Step 2: Capture Evidence

Capture the same surfaces every time:

1. Health snapshot (timestamped).
2. Robot events around incident window.
3. Search excerpts for resize/reflow markers.
4. Deterministic scenario replay artifacts (`R1-R4`) when reproducible.
5. Watchdog/degradation state summary and the exact control toggles applied.

Store evidence under a unique incident directory, for example:

`evidence/wa-1u90p.8.4/<incident-id>/`

### Step 3: Classify Fault and Decide Rollback Mode

Use fault classes from `docs/resize-artifact-fault-model-wa-1u90p.4.1.md`:

- `F1` PTY-terminal mismatch window: prioritize emergency disable + legacy fallback.
- `F2` Non-atomic render reads: prioritize exposure reduction and render-path rollback.
- `F3` Tab fanout split inconsistency: prioritize rollback and fanout concurrency mitigation gates.

Rollback mode selection:

1. `Fast rollback` (default for `SEV-1`): immediate legacy fallback.
2. `Cohort rollback` (default for `SEV-2`): step down one cohort and hold.
3. `Hold-only` (`SEV-3`): no feature rollback, but freeze expansion and require green re-validation.

### Step 4: Verify Rollback Posture

After applying rollback posture:

1. Re-run deterministic gate scenarios (`R1-R3` minimum).
2. Confirm no critical artifacts or invariant failures.
3. Confirm p95/p99 returns within accepted fallback thresholds.
4. Publish checkpoint result as `GO` (resume), `HOLD` (stay frozen), or `ROLLBACK` (remain on legacy).

## Rollback Checklist

1. Incident ticket opened with UTC timestamp.
2. Rollout frozen and cohort recorded.
3. Emergency disable/fallback toggles applied (if required).
4. Evidence directory created and populated.
5. Post-rollback verification run completed.
6. Decision logged with approver sign-off.

## Communication Template

```md
## Resize Incident Update
- Incident ID:
- Date/Time (UTC):
- Severity: SEV-1 | SEV-2 | SEV-3
- Cohort at trigger:
- Trigger condition:
- Current posture:
  - emergency_disable:
  - legacy_fallback_enabled:
  - active cohort:
- Evidence bundle:
- Decision: GO | HOLD | ROLLBACK
- Next checkpoint (UTC):
```

## Post-Incident Exit Criteria

Before rollout progression resumes:

1. Trigger condition is resolved or explicitly mitigated.
2. Compatibility and SLO gates pass in fallback/current posture.
3. Required approver signs off on resumption.
4. Follow-up engineering beads are created and linked to incident evidence.

## Exit Criteria for `wa-1u90p.8.4`

1. Runbook provides an actionable operator path from detection to rollback verification.
2. Severity mapping, ownership, and SLAs are explicit.
3. Evidence capture is standardized and reusable for `wa-1u90p.8.6` go/no-go review.
