# Resize/Reflow User Release and Tuning Guidance (`wa-1u90p.8.5`)

Date: 2026-02-20  
Status: Draft v1 user/operator guidance by hardware tier  
Depends on: `wa-1u90p.8.1`, `wa-1u90p.8.2`, `wa-1u90p.8.4`, `wa-1u90p.4.5`, `wa-1u90p.4.7`

## Purpose

Provide practical release and tuning guidance for resize/reflow behavior across low, mid, and high hardware tiers.

This document is user-facing and operational. It tells operators what to expect, what to tune first, and when to fall back.

Read together with:
- `docs/resize-no-regression-compatibility-contract-wa-1u90p.8.1.md`
- `docs/resize-rollout-plan-wa-1u90p.8.2.md`
- `docs/resize-incident-response-rollback-runbook-wa-1u90p.8.4.md`
- `docs/resize-performance-slos.md`

## Hardware Tier Mapping

Use these tier labels consistently in release notes, support triage, and rollout decisions.

| Tier | Typical host profile | Expected resize posture |
|---|---|---|
| `low` | <= 4 CPU cores, constrained memory bandwidth, integrated graphics | Prioritize responsiveness and stability over visual polish under stress |
| `mid` | 6-10 modern CPU cores, mainstream laptop/desktop memory profile | Balanced quality and latency |
| `high` | >= 12 modern CPU cores, high memory bandwidth/workstation class | Highest quality while preserving latency SLOs |

## User-Visible Targets by Tier

These targets are derived from `docs/resize-performance-slos.md` and should be reflected in user-facing expectations.

| Tier | End-to-end resize latency target |
|---|---|
| `low` | p95 <= 33ms, p99 <= 50ms |
| `mid` | p95 <= 20ms, p99 <= 33ms |
| `high` | p95 <= 12ms, p99 <= 20ms |

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

- Use Profile A or B only
- No unresolved P0/P1 resize incidents
- Keep rollback controls immediately available

### Canary/Beta channel

- Start with Profile B
- Promote to Profile C only after sustained green windows
- Any compatibility invariant breach forces immediate rollback posture

## Fast Troubleshooting Checklist

When users report resize hitches/artifacts, collect this in order:

```bash
ft status --health
ft robot events --limit 50
ft robot search "resize OR reflow OR watchdog OR emergency_disable" --limit 100
```

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

## User-Facing Release Notes Template

Use this text block for release announcements:

```md
### Resize/Reflow Behavior Update

This release includes resize/reflow pipeline improvements with hardware-tier-aware defaults.

What to expect:
- Low-tier systems: stability-first behavior under heavy resize storms
- Mid-tier systems: balanced responsiveness and visual quality
- High-tier systems: highest quality mode with latency safeguards

If you observe regressions:
1. Capture health + event snapshots (`ft status --health`, `ft robot events`)
2. Share your hardware tier and reproduction steps
3. Apply fallback profile per runbook if interaction continuity is impacted
```

## Exit Criteria for `wa-1u90p.8.5`

1. User-facing guidance exists for low/mid/high hardware tiers with explicit expectations.
2. Tuning profiles are documented with practical tradeoffs.
3. Troubleshooting and fallback paths align with rollout/runbook contracts.
4. Guidance is ready to be referenced by final go/no-go evidence in `wa-1u90p.8.6`.
