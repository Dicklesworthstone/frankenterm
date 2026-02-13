# Resize Baseline Bottleneck Dossier (`wa-1u90p.1.5`)

Date: 2026-02-13  
Author: `GentleBrook`  
Parent track: `wa-1u90p.1`  
Status: Draft scaffold (final ranking calibration pending `wa-1u90p.1.3` closeout evidence)

## Scope
This dossier is the ranking and sequencing source of truth for resize/reflow interventions.
It converts baseline artifacts into an actionable implementation queue with explicit impact, risk, and confidence.

## Inputs (Current Baseline Surfaces)
- Scenario and timeline contract: `docs/resize-baseline-scenarios.md`
- SLO thresholds and gate definitions: `docs/resize-performance-slos.md`
- Lock/memory telemetry surfaces: `crates/frankenterm-core/src/runtime.rs`
- Reusable component opportunities: `docs/frankentui-reusable-component-porting-matrix.md`

## Ranking Method
Each intervention receives five scores (`1..5`):
- `impact`: estimated p95/p99 latency or artifact reduction against M1/M2/M3
- `confidence`: evidence quality from existing traces/tests/profiles
- `effort`: implementation complexity and integration churn
- `risk`: regression probability (semantic/render correctness)
- `dependency`: how blocked it is on upstream evidence/tasks

Computed values:
- `priority_score = (impact * confidence) / max(1, effort)`
- `execution_score = priority_score - (risk * 0.35) - (dependency * 0.25)`

Interpretation:
- Higher `execution_score` should be scheduled earlier.
- Any item with `dependency >= 4` is treated as prework-only until blockers clear.

## Baseline Bottleneck Lanes
- `B1`: scheduler queueing inflation (`scheduler_queueing`)
- `B2`: reflow CPU tail latency (`logical_reflow`)
- `B3`: render preparation spikes (`render_prep`)
- `B4`: presentation jitter and transient artifacts (`presentation`)
- `B5`: lock/memory pressure coupling from runtime telemetry

## Ranked Intervention Candidates (Initial)

| Rank | Intervention | Primary bottleneck lanes | Impact | Confidence | Effort | Risk | Dependency | Execution score | Expected effect |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|
| 1 | Two-phase resize transaction + latest-intent wins queueing | `B1`, `B4` | 5 | 4 | 3 | 3 | 3 | 5.20 | Reduce queue buildup and stale-frame presentation under resize storms |
| 2 | Viewport-first incremental reflow with background completion | `B2`, `B4` | 5 | 3 | 4 | 4 | 3 | 3.55 | Improve interaction p95/p99 while preserving eventual full correctness |
| 3 | Adaptive buffer headroom/shrink policy in resize hot paths | `B2`, `B3`, `B5` | 4 | 4 | 2 | 2 | 2 | 7.30 | Lower allocator churn and reflow jitter in oscillating resize workloads |
| 4 | Dirty-span-aware render diffing (row + span granularity) | `B3`, `B4` | 4 | 3 | 4 | 4 | 3 | 2.55 | Cut render prep work for sparse updates and reduce transient mismatch risk |
| 5 | Strategy selector for diff/repaint policy by live pressure | `B3`, `B5` | 3 | 3 | 3 | 3 | 3 | 2.40 | Prevent pathological repaint choices under bursty load |
| 6 | Cursor snapshot memory retention controls and warning escalation | `B5` | 3 | 4 | 2 | 2 | 1 | 4.85 | Bound memory tail risk and tighten recovery triggers |

Notes:
- Candidates 3-5 map directly to component opportunities already documented in `docs/frankentui-reusable-component-porting-matrix.md`.
- Candidate 1 is structurally aligned with downstream `wa-1u90p.2.*` control-plane work.

## Evidence Gaps Blocking Final Ranking
- Missing finalized lock contention and memory growth curves from `wa-1u90p.1.3`.
- Missing consolidated percentile rollups and artifact incidence trend lines expected in this bead (`wa-1u90p.1.5`).
- Missing sustained soak trend export tied to hardware tier matrix (`low`/`mid`/`high`).

## Proposed Execution Sequence (When Blockers Clear)
1. Finalize `wa-1u90p.1.3` outputs and attach numeric lock/memory deltas.
2. Recompute `execution_score` with real tiered p95/p99 deltas from baseline artifacts.
3. Promote top 2 interventions into immediate implementation beads with explicit success metrics.
4. Gate rollout by SLO contract checks and nightly trend regressions.

## Artifact Requirements to Close `wa-1u90p.1.5`
- Per workload class (`R1..R4`) percentile rollups for `M1` + per-stage `M2` lanes.
- Artifact incidence report aligned to `M3` thresholds.
- Lock/memory telemetry summary from runtime health surfaces (`B5` lane).
- Ranked intervention table with updated numeric deltas and final confidence rationale.
- Sequencing decision note referencing blocked/unblocked downstream beads.

## Revision Log
- 2026-02-13: Initial dossier scaffold with scoring rubric, first-pass intervention ranking, and closeout artifact checklist.
