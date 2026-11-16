# Staged Resize/Reflow Rollout Plan (`wa-1u90p.8.2`)

Date: 2026-02-15  
Status: Baseline rollout contract for resize/reflow release-track execution  
Unblocks: `wa-1u90p.8.4`, `wa-1u90p.8.6`, `wa-1u90p.8.7`

## Purpose

Define a deterministic rollout path for resize/reflow behavior changes with:

- canary cohorts and explicit exposure limits
- measurable phase entry/exit checkpoints
- instant rollback criteria tied to compatibility and SLO contracts
- reusable communication and approval workflow

This plan depends on:

- `docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md`
- `docs/resize-performance-slos.md`
- `docs/resize-baseline-scenarios.md`
- `docs/resize-artifact-fault-model-wa-1u90p.4.1.md`
- `docs/resize-incident-response-rollback-runbook-wa-1u90p.8.4.md`
- `docs/resize-user-facing-release-tuning-guidance-wa-1u90p.8.5.md`

## Rollout Control Surface

The following controls gate resize/reflow behavior during rollout:

| Control | Default | Rollout use |
|---|---:|---|
| `ResizeSchedulerConfig.control_plane_enabled` | `true` | Global enable/disable for resize control-plane scheduling |
| `ResizeSchedulerConfig.emergency_disable` | `false` | Hard kill-switch to suppress control-plane behavior |
| `ResizeSchedulerConfig.legacy_fallback_enabled` | `true` | Allows immediate fallback to legacy path when disabled |
| `ResizeSchedulerConfig.input_guardrail_enabled` | `true` | Preserves interaction responsiveness under resize pressure |
| `ResizeSchedulerConfig.domain_budget_enabled` | `false` | Optional fairness gate during cross-domain storms |
| `ResizeSchedulerConfig.max_pending_panes` | `128` | Backpressure threshold for overload handling |
| `ResizeDegradationTier` ladder | `full_quality` | Controlled degradation order before emergency compatibility |
| Runtime watchdog assessment | enabled | Stalled-transaction / safe-mode health gating |

## Cohorts

| Cohort | Exposure goal | Primary risk focus |
|---|---:|---|
| `C0` Internal simulation-only | 0% user traffic | Contract correctness and deterministic replay |
| `C1` Internal operator canary | <= 10% active operator sessions | Artifact regressions and interactive latency spikes |
| `C2` Controlled beta | 10-40% sessions | Long-haul stability and rollback responsiveness |
| `C3` Broad rollout | >= 90% sessions | Fleet-wide consistency and operational maintainability |

## Phase Plan

### Phase 0: Safe Baseline

Target posture:
- keep rollout exposure at `C0`
- use compatibility + SLO suites as release gate only

Exit criteria to Phase 1:
1. `wa-1u90p.8.1` compatibility contract is closed and green.
2. `wa-1u90p.7.4` performance regression suite is active in CI.
3. Baseline scenario suite (`R1-R4`) has reproducible artifacts.

### Phase 1: Internal Canary

Target posture:
- enable canary exposure for `C1` only
- keep emergency kill-switch runbook primed

Entry criteria:
1. Phase 0 exit criteria met.
2. On-call owner + rollback approver assigned.
3. Kill-switch and fallback controls are validated in a dry run.

Exit criteria to Phase 2:
1. Zero critical compatibility invariant failures.
2. `M1`/`M2` p95 targets hold for canary windows.
3. No unresolved resize/reflow P0 incidents.

### Phase 2: Controlled Beta

Target posture:
- expand to `C2` cohorts
- enforce stricter checkpoint frequency and evidence retention

Entry criteria:
1. Phase 1 stable window complete (minimum 7 consecutive green days).
2. Incident response path from `wa-1u90p.8.4` is runnable.
3. Degradation ladder transitions are observable and audited.

Exit criteria to Phase 3:
1. No-go triggers remain silent for 14 consecutive days.
2. Latency and artifact budgets remain within approved thresholds.
3. Rollback drill completes within operator response budget.

### Phase 3: Broad Rollout

Target posture:
- expand to `C3`
- keep rollback controls and compatibility gates mandatory

Steady-state requirements:
1. Compatibility + SLO gates remain release blockers.
2. Emergency disable and fallback path remain one-step executable.
3. Any critical incident immediately freezes further expansion.

## Decision Checkpoints

At each phase transition, publish:

1. Compatibility contract status (`wa-1u90p.8.1` invariants).
2. SLO compliance snapshot (`M1-M4`, artifacts, crash budget).
3. Resize watchdog and degradation ladder summary.
4. Open incident/bug status for resize/reflow labels.
5. Explicit decision outcome: `GO`, `HOLD`, or `ROLLBACK`.

## Explicit Rollback Criteria

Rollback is mandatory when any condition is true:

1. Any critical compatibility invariant failure (`RC-ALTSCREEN-001`, `RC-INTERACTION-001`, `RC-LIFECYCLE-001`).
2. Critical artifact count > 0 in gate scenarios.
3. `M1` p99 exceeds target by >20% in two consecutive runs.
4. Any CI/nightly resize scenario crash or hang.
5. Watchdog signals sustained critical stall state or emergency safe-mode activation.

## Rollback Procedure (Fast Path)

Detailed operator actions, severity mapping, and evidence templates are defined in:
- `docs/resize-incident-response-rollback-runbook-wa-1u90p.8.4.md`

1. Freeze rollout progression immediately.
2. Enable `emergency_disable=true` and verify `legacy_fallback_enabled=true`.
3. Re-run deterministic `R1-R3` suites on fallback posture.
4. Publish rollback notice with trigger, timestamp, and affected cohort.
5. Open/attach incident evidence bundle for `wa-1u90p.8.6` go/no-go review.

## Communication Checklist

Before phase transition:
1. Announce cohort scope, window, owner, approver.
2. Link evidence packet and rollback owner.

During transition:
1. Post checkpoint updates on fixed cadence.
2. Report trigger breaches immediately.

After transition:
1. Publish final decision (`GO`/`HOLD`/`ROLLBACK`).
2. Record follow-up owners and deadlines.

## Decision Log Template

```md
## Resize Rollout Decision: <phase transition>
- Date/Time (UTC):
- Change owner:
- Approver(s):
- Cohort scope:
- Evidence packet:
  - compatibility gate status:
  - SLO snapshot:
  - watchdog/degradation summary:
  - open incidents:
- Decision: GO | HOLD | ROLLBACK
- If rollback:
  - trigger(s):
  - rollback posture:
  - completion timestamp:
- Follow-up actions:
```

## Exit Criteria for `wa-1u90p.8.2`

1. Phase 0-3 posture and cohorting rules are explicitly documented.
2. Entry/exit checkpoints and explicit rollback criteria are objective and measurable.
3. Fast-path rollback procedure is documented and operator-oriented.
4. Downstream beads (`wa-1u90p.8.4`, `wa-1u90p.8.6`, `wa-1u90p.8.7`) can execute without rediscovering rollout assumptions.
