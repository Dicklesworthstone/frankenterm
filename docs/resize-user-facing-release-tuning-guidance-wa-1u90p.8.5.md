# Resize/Reflow User Release and Tuning Guidance (`wa-1u90p.8.5`)

Date: 2026-03-14
Status: Draft v2 user/operator guidance by hardware tier; evidence-backed but not yet promotion-ready because `ft-1u90p.8.7` remains `HOLD` as of the 2026-03-14 checkpoint and 2026-03-15 guardrail validation rerun
Depends on: `wa-1u90p.8.1`, `wa-1u90p.8.2`, `wa-1u90p.8.4`, `wa-1u90p.4.5`, `wa-1u90p.4.7`, `ft-1u90p.8.7`

## Purpose

Provide practical release and tuning guidance for resize/reflow behavior across low, mid, and high hardware tiers.

This document is user-facing and operational. It tells operators what to expect, what to tune first, and when to fall back.

Read together with:
- `docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md`
- `docs/resize-rollout-plan-wa-1u90p.8.2.md`
- `docs/resize-incident-response-rollback-runbook-wa-1u90p.8.4.md`
- `docs/resize-performance-slos.md`
- `docs/resize-controlled-beta-feedback-loop-wa-1u90p.8.7.md`

## Current Release Posture (2026-03-14 Checkpoint)

This guidance is now evidence-backed, but it is not a GA promotion document yet.

The current `ft-1u90p.8.7` checkpoint remains `HOLD` because the beta loop still lacks real-user cohort coverage across low/mid/high hardware tiers. The latest guardrail rerun on 2026-03-15 UTC revalidated the evidence package and confirmed that the only defensible posture is still hold-and-collect, not broaden-and-promote.

Current evidence snapshot:

- decision: `HOLD`
- resize events observed: `66` vs target `500/day`
- alt-screen transitions observed: `10` vs target `50/day`
- sessions by tier: `low=0`, `mid=0`, `high=0`, `unknown=1`
- real-user feedback items: `0`
- latest validation log: `tests/e2e/logs/ft_1u90p_8_7_20260314_204826.jsonl`

Operational consequence:

- use this document to guide internal rollout posture, beta handling, and support responses
- do not treat Profile C as a generally recommended default until `ft-1u90p.8.7` clears the sample-sufficiency and real-user-feedback anomalies
- keep rollback/fallback controls immediately available on every tier

## Hardware Tier Mapping

Use these tier labels consistently in release notes, support triage, and rollout decisions.

| Tier | Typical host profile | Expected resize posture |
|---|---|---|
| `low` | <= 4 CPU cores, constrained memory bandwidth, integrated graphics | Prioritize responsiveness and stability over visual polish under stress |
| `mid` | 6-10 modern CPU cores, mainstream laptop/desktop memory profile | Balanced quality and latency |
| `high` | >= 12 modern CPU cores, high memory bandwidth/workstation class | Highest quality while preserving latency SLOs |

Until the beta cohort contains real-user samples from all three tiers, treat these mappings as operational planning categories rather than promotion evidence.

## User-Visible Targets by Tier

These targets are derived from `docs/resize-performance-slos.md` and should be reflected in user-facing expectations.

| Tier | End-to-end resize latency target |
|---|---|
| `low` | p95 <= 33ms, p99 <= 50ms |
| `mid` | p95 <= 20ms, p99 <= 33ms |
| `high` | p95 <= 12ms, p99 <= 20ms |

These remain target envelopes, not currently achieved cohort-wide outcomes. The active checkpoint is still fixture-only for perception data and cannot justify broader rollout on its own.

## Tier-by-Tier Rollout Posture

Use the matrix below for current operator decisions while `ft-1u90p.8.7` is still `HOLD`.

| Tier | Current recommended profile | Channel posture | Promotion stance | First fallback step |
|---|---|---|---|---|
| `low` | Profile A | Internal/canary only | Do not broaden beyond controlled cohorts | Set `ResizeSchedulerConfig.emergency_disable=true` and keep `legacy_fallback_enabled=true` |
| `mid` | Profile B | Internal/canary only | Hold until real-user cohort evidence exists | Step down to Profile A before considering wider rollback |
| `high` | Profile B by default; Profile C only for explicit local experiments | Internal/canary only | Do not advertise Profile C as recommended yet | Step back to Profile B, then use emergency disable if user-visible regressions persist |

## Recommended Tuning Profiles

Use these defaults at rollout start. Move one profile step at a time and re-check deterministic baseline scenarios (`R1-R3`) after each change.

### Profile A: Conservative (Low-Tier Safety)

Recommended for low-tier hosts or unstable beta cohorts.

- `ResizeSchedulerConfig.domain_budget_enabled=true`
- `ResizeSchedulerConfig.max_pending_panes=64`
- `ResizeSchedulerConfig.input_guardrail_enabled=true`
- Keep `ResizeSchedulerConfig.legacy_fallback_enabled=true`

Expected tradeoff:
- Fewer visual refinements during storms
- Better interaction continuity under pressure

### Profile B: Balanced (Default Mid-Tier)

Recommended default for most operators.

- `ResizeSchedulerConfig.domain_budget_enabled=true`
- `ResizeSchedulerConfig.max_pending_panes=128`
- `ResizeSchedulerConfig.input_guardrail_enabled=true`
- Keep `ResizeSchedulerConfig.legacy_fallback_enabled=true`

Expected tradeoff:
- Good quality/latency balance
- Predictable recovery from bursty resize activity

### Profile C: Performance/Quality (High-Tier)

Recommended for high-tier hosts after canary validation.

- `ResizeSchedulerConfig.domain_budget_enabled=false`
- `ResizeSchedulerConfig.max_pending_panes=256`
- `ResizeSchedulerConfig.input_guardrail_enabled=true`
- Keep `ResizeSchedulerConfig.legacy_fallback_enabled=true`

Expected tradeoff:
- Best visual refinement potential
- Requires healthy headroom and active monitoring

## Release Channel Guidance

### Stable channel

- Do not widen the resize/reflow rollout solely on the basis of the current evidence package
- If stable users must be supported during investigation, constrain guidance to rollback/fallback steps and conservative tuning only
- No unresolved P0/P1 resize incidents
- Keep rollback controls immediately available
- Do not recommend Profile C on stable while `ft-1u90p.8.7` is `HOLD`

### Canary/Beta channel

- Low-tier cohorts start with Profile A
- Mid-tier cohorts start with Profile B
- High-tier cohorts may locally evaluate Profile C only after verifying tier headroom and keeping rollback controls one step away
- Promote to broader exposure only after `ft-1u90p.8.7` sample sufficiency and real-user feedback gates are satisfied
- Any compatibility invariant breach forces immediate rollback posture

## Operator Playbooks by Tier

### Low Tier

Use this when a host is CPU- or memory-constrained, on integrated graphics, or already showing interaction-pressure symptoms.

- Start on Profile A and leave `domain_budget_enabled=true`
- Treat user-perceived hitching as a stability issue first, not a polish issue
- If p99 exceeds `50ms` with visible interaction impact, do not tune upward; fall back immediately
- If alt-screen regressions or stale/blank frames appear, go directly to emergency disable + legacy fallback

### Mid Tier

Use this for mainstream laptops/desktops where balanced behavior is the main goal.

- Start on Profile B
- If users report hitching but no correctness regressions, first reduce queue pressure by stepping to Profile A
- If repeated `P1`/`P2`-class symptoms occur across sessions, stop local tuning and follow the rollback runbook
- Do not treat one clean synthetic run as evidence for wider exposure

### High Tier

Use this for workstation-class systems with clear headroom and active operator monitoring.

- Start on Profile B, not Profile C
- Only test Profile C in explicit canary windows with artifact capture and rollback readiness
- If Profile C increases p99 or creates visual regressions, return to Profile B immediately rather than trying intermediate ad hoc tuning
- Do not recommend Profile C in release notes until real-user high-tier cohort evidence exists

## Fast Troubleshooting Checklist

When users report resize hitches/artifacts, collect this in order:

```bash
ft status --health
ft robot events --limit 50
ft robot search "resize OR reflow OR watchdog OR emergency_disable" --limit 100
```

Then map the report to the current evidence package:

- compare the symptom against `docs/resize-controlled-beta-feedback-loop-wa-1u90p.8.7.md` anomaly categories (`A1`..`A5`)
- check `evidence/wa-1u90p.8.7/cohort_daily_summary.json` for current open blockers and owners
- use `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv` to determine whether the issue already has a telemetry/feedback join
- consult `evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md` for the current checkpoint decision and close-loop status

If reproduction is required, capture deterministic artifacts:

```bash
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json --resize-timeline-json
ft simulate run fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml --json --resize-timeline-json
```

## Escalation and Fallback Guidance

Immediate fallback to legacy behavior is recommended when any of these occur:

1. Any compatibility invariant breach (`RC-ALTSCREEN-001`, `RC-INTERACTION-001`, `RC-LIFECYCLE-001`)
2. Reproducible critical visual artifact (blank/stale frame)
3. Sustained p99 regression beyond tier target with user-visible impact

Operational action:
- Set `ResizeSchedulerConfig.emergency_disable=true`
- Ensure `ResizeSchedulerConfig.legacy_fallback_enabled=true`
- Follow the full runbook in `docs/resize-incident-response-rollback-runbook-wa-1u90p.8.4.md`

## Evidence Map for Operators

Use these artifacts when support, rollout, or release-note decisions need proof rather than intuition.

| Need | Artifact |
|---|---|
| Current beta status and anomaly ledger | `evidence/wa-1u90p.8.7/cohort_daily_summary.json` |
| Current dated checkpoint and rationale | `evidence/wa-1u90p.8.7/decision_checkpoint_20260314.md` |
| Feedback-to-telemetry join state | `evidence/wa-1u90p.8.7/telemetry_feedback_correlation.csv` |
| Source feedback log | `evidence/wa-1u90p.8.7/beta_feedback_log.jsonl` |
| Guardrail validation run | `tests/e2e/logs/ft_1u90p_8_7_20260314_204826.jsonl` |
| Beta-loop contract and rubric | `docs/resize-controlled-beta-feedback-loop-wa-1u90p.8.7.md` |

## User-Facing Release Notes Template

Use this text block for release announcements:

```md
### Resize/Reflow Behavior Update

This release includes resize/reflow pipeline improvements with hardware-tier-aware defaults.

Current rollout posture:
- promotion remains on `HOLD` pending real-user cohort evidence across low/mid/high hardware tiers
- the guidance below is safe-to-operate tuning guidance, not a signal that broader rollout gates are cleared

What to expect:
- Low-tier systems: stability-first behavior under heavy resize storms
- Mid-tier systems: balanced responsiveness and visual quality
- High-tier systems: balanced mode by default; highest-quality mode remains canary-only until beta evidence clears

If you observe regressions:
1. Capture health + event snapshots (`ft status --health`, `ft robot events`)
2. Share your hardware tier and reproduction steps
3. Apply fallback profile per runbook if interaction continuity is impacted
```

## Exit Criteria for `wa-1u90p.8.5`

1. User-facing guidance exists for low/mid/high hardware tiers with explicit expectations and current rollout posture.
2. Tuning profiles are documented with practical tradeoffs plus evidence-backed restrictions while beta remains `HOLD`.
3. Troubleshooting and fallback paths align with rollout/runbook contracts and point operators to the active evidence artifacts.
4. Guidance is ready to be referenced by final go/no-go evidence in `wa-1u90p.8.6` once `ft-1u90p.8.7` clears its sample-sufficiency and real-user-feedback blockers.
