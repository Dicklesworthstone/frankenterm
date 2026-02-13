# Resize/Reflow Performance SLO Contract

**Parent track:** `wa-1u90p.1`  
**Related tasks:** `wa-1u90p.1.2`, `wa-1u90p.1.3`, `wa-1u90p.1.4`, `wa-1u90p.1.5`  
**Status:** Draft v0.2 (instrumentation mapped; final calibration pending `wa-1u90p.1.3` + `wa-1u90p.1.5`)

This document defines hard SLO targets and release gates for resize/reflow behavior.
It is the authoritative baseline used by CI, soak, and go/no-go review for the zero-hitch resize program.

## Scope

In scope:
- interactive resize latency from user intent to presentation
- stage-level latency for scheduler/reflow/render phases
- artifact incidence budgets under deterministic storm scenarios
- crash-free operation and rollback/degradation triggers

Out of scope:
- semantic correctness of non-resize terminal behavior (tracked separately)
- renderer quality improvements unrelated to resize/reflow latency

## Workload Classes

Workload classes map directly to deterministic scenarios in `docs/resize-baseline-scenarios.md`.

| Class | Scenario anchor | Description |
|---|---|---|
| `R1` | `resize_single_pane_scrollback` | Single pane + heavy scrollback resize sweep |
| `R2` | `resize_multi_tab_storm` | Multi-tab/multi-pane repeated resize storm |
| `R3` | `font_churn_multi_pane` | Font-size churn mixed with resize |
| `R4` | `mixed_scale_soak` | Long-running mixed resize/font/scrollback soak |

## Hardware Tiers

| Tier | Definition (operator-side) |
|---|---|
| `low` | 4 cores or fewer, integrated graphics, constrained memory |
| `mid` | 6-10 modern cores, standard laptop/desktop profile |
| `high` | 12+ modern cores, high sustained single-thread + memory bandwidth |

## Measurement Sources (Implemented)

| SLO lane | Implemented source | Access path today | Notes |
|---|---|---|---|
| Scenario identity and reproducibility | `Scenario::metadata`, `Scenario::reproducibility_key()` in `crates/frankenterm-core/src/simulation.rs` | `ft simulate validate <scenario>.yaml --json`, `ft simulate run <scenario>.yaml --json` | Stable and machine-readable now |
| Per-event resize latency | `ResizeTimelineEvent.total_duration_ns` from `execute_all_with_resize_timeline` | Programmatic/test harness using `Scenario::execute_all_with_resize_timeline` | Source of truth for M1 p50/p95/p99 |
| Stage latency and queue attribution | `ResizeTimelineStageSample` + `ResizeQueueMetrics` + `ResizeTimeline::stage_summary()` | Programmatic/test harness | Stage names: `input_intent`, `scheduler_queueing`, `logical_reflow`, `render_prep`, `presentation` |
| Flamegraph rows for attribution | `ResizeTimeline::flame_samples()` | Programmatic/test harness | Used for hotspot inspection and regression triage |
| Lock contention and hold-time telemetry | `RuntimeMetrics::{avg,max}_storage_lock_wait_ms`, `storage_lock_contention_events`, `RuntimeMetrics::{avg,max}_storage_lock_hold_ms` in `crates/frankenterm-core/src/runtime.rs` | Runtime health snapshot path (`RuntimeHandle::update_health_snapshot`) | Current warning thresholds: wait `15.0ms`, hold `75.0ms` |
| Cursor snapshot memory telemetry | `RuntimeMetrics::{cursor_snapshot_bytes_last,cursor_snapshot_bytes_max,avg_cursor_snapshot_bytes}` | Runtime health snapshot path | Current warning threshold: `64 MiB` |

## Collection Commands (Current Tree)

```bash
# Scenario metadata + reproducibility key
ft simulate validate fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json

# Full resize timeline artifact envelope (timeline + stage_summary + flame_samples)
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json --resize-timeline-json

# Resize timeline schema/probe coverage
cargo test -p frankenterm-core simulation_resize_suite -- --nocapture
cargo test -p frankenterm-core resize_timeline_summary_and_flame_samples_cover_all_stages -- --nocapture

# Lock/memory warning-threshold coverage
cargo test -p frankenterm-core warning_threshold_fires -- --nocapture
```

## Primary SLO Metrics

### M1. End-to-End Interaction Latency

Measured as `input_intent -> presentation` from resize timeline artifacts.

| Tier | p50 | p95 | p99 |
|---|---:|---:|---:|
| `low` | <= 20 ms | <= 33 ms | <= 50 ms |
| `mid` | <= 12 ms | <= 20 ms | <= 33 ms |
| `high` | <= 8 ms | <= 12 ms | <= 20 ms |

### M2. Stage Latency Budgets (`R2`, `R3` focus)

Measured per stage using timeline probes.

| Stage | low p95 | mid p95 | high p95 |
|---|---:|---:|---:|
| `input_intent` | <= 2 ms | <= 1 ms | <= 1 ms |
| `scheduler_queueing` | <= 6 ms | <= 3 ms | <= 2 ms |
| `logical_reflow` | <= 14 ms | <= 8 ms | <= 5 ms |
| `render_prep` | <= 10 ms | <= 6 ms | <= 4 ms |
| `presentation` | <= 8 ms | <= 4 ms | <= 3 ms |

### M3. Visual Artifact Budgets

Artifacts are counted per resize-class event.

| Class | Allowed incidence |
|---|---|
| Critical artifacts (blank frame, stale full-frame, severe tear) | 0 per run |
| Minor artifacts (single-frame transient mismatch) | <= 0.1% of resize-class events |

### M4. Crash/Recovery Budget

| Context | Requirement |
|---|---|
| CI scenario runs | 0 crashes, 0 hangs |
| Nightly soak (`R4`) | >= 99.95% crash-free session-hours |
| Release candidate | 0 unresolved resize/reflow P0 incidents |

## Gate Definitions

## CI Gate (required on merge path)

- Run deterministic scenario suite in `crates/frankenterm-core/tests/simulation_resize_suite.rs`.
- Run probe integrity tests for timeline summaries/flame rows.
- Run runtime warning-threshold tests for lock wait/hold and cursor snapshot memory.
- Enforce numeric M1/M2/M3 thresholds from generated baseline artifacts (mid tier minimum).
- Fail build on any critical artifact or threshold breach.

## Nightly/Soak Gate

- Run `R4` mixed-scale soak at minimum 60 minutes on low+mid tiers.
- Enforce M1 p99, M3, and M4 crash-free requirements.
- Store trend artifacts for 7-day regression review.

## Go/No-Go Gate

- Require green CI + 7 consecutive green nightly runs.
- Require no unresolved resize/reflow P0/P1 bugs.
- Require lock/memory profile review sign-off (`wa-1u90p.1.3` evidence).

## Artifact Contract

Required outputs for each gate:
- scenario metadata + reproducibility key (`Scenario::metadata`, `Scenario::reproducibility_key`)
- raw timeline events (`ResizeTimeline.events[*].total_duration_ns` for M1 percentile derivation)
- stage samples (`ResizeTimeline.events[*].stages[*].duration_ns`)
- queue attribution (`ResizeTimeline.events[*].stages[*].queue_metrics.depth_before/depth_after`)
- stage aggregates (`ResizeTimeline::stage_summary()` -> `avg_duration_ns`, `p95_duration_ns`, `max_duration_ns`)
- flame rows (`ResizeTimeline::flame_samples()`)
- lock/memory telemetry (`max_storage_lock_wait_ms`, `avg_storage_lock_wait_ms`, `storage_lock_contention_events`, `max_storage_lock_hold_ms`, `cursor_snapshot_bytes_last`, `cursor_snapshot_bytes_max`)
- health warning snapshot (`HealthSnapshot.warnings`) + crash/timeout report

## Current Gaps and Near-Term Closure

- Full timeline artifacts are now available via `ft simulate run --json --resize-timeline-json`; plain `--json` mode remains metadata/event playback oriented.
- Numeric p50/p99 reduction and artifact incidence rollups are expected deliverables of `wa-1u90p.1.5` (see `docs/resize-baseline-bottleneck-dossier.md`).
- Until `wa-1u90p.1.5` lands, this document remains the normative threshold contract and the artifact schema baseline.

## Degradation and Rollback Policy

## Trigger conditions

Any of the following triggers mitigation:
- M1 p99 exceeds tier target by >20% in 2 consecutive runs
- any critical artifact appears
- any crash/hang in CI scenario run

## Mitigation sequence

1. Enable/raise degradation mode for resize quality policy.
2. Re-run deterministic `R1-R3` scenarios to validate containment.
3. If still failing, rollback latest resize/reflow optimization slice.

## Recovery criteria

Rollback/degradation can be lifted only after:
- 3 consecutive green CI runs
- 2 consecutive green nightly runs on affected tier(s)

## Dependency-Bound Sections

These thresholds are explicit and enforceable now, but expected to be tightened
after remaining baseline tasks land:
- `wa-1u90p.1.2`: stage-level instrumentation completeness and queue metrics
- `wa-1u90p.1.3`: lock contention + memory attribution under resize storms

When those tasks close, update this document with final calibrated values and
record the revision in bead notes.
