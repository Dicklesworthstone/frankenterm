# Resize Baseline Bottleneck Dossier (`wa-1u90p.1.5`)

Date: 2026-02-14  
Author: `MaroonGlacier` (interim pass; based on prior scaffold by `GentleBrook`)  
Parent track: `wa-1u90p.1`  
Status: In progress, dependency-bound. Compile and scenario replay blockers from `wa-1u90p.1.3` are cleared; this revision adds fresh timeline rollups and narrows remaining closure gap to live lock/memory percentile capture.

## Scope
This dossier is the source of truth for ranking and sequencing resize/reflow interventions.  
It translates baseline instrumentation, SLO contracts, and known lock/render behavior into an implementation order with explicit dependency and risk accounting.

## Inputs and Evidence Surfaces
- Scenario pack + schema contract: `docs/resize-baseline-scenarios.md`
- SLO/gate contract: `docs/resize-performance-slos.md`
- Transaction semantics (normative): `docs/adr/0011-resize-transaction-state-machine.md`
- Existing lock/async pane analysis: `docs/resize-lock-graph-wa-1u90p.5.7.md`, `docs/resize-async-model-wa-1u90p.5.8.md`, `docs/pty-resize-fault-isolation-wa-1u90p.5.4.md`
- Resize timeline model and summaries:
  - `crates/frankenterm-core/src/simulation.rs:167`
  - `crates/frankenterm-core/src/simulation.rs:314`
  - `crates/frankenterm-core/src/simulation.rs:707`
- Timeline artifact envelope output path:
  - `crates/frankenterm/src/main.rs:22109`
- Runtime lock/memory telemetry and warning thresholds:
  - `crates/frankenterm-core/src/runtime.rs:376`
  - `crates/frankenterm-core/src/runtime.rs:444`
  - `crates/frankenterm-core/src/runtime.rs:2260`
  - `crates/frankenterm-core/src/runtime.rs:2501`
- Reusable render/reflow interventions:
  - `docs/frankentui-reusable-component-porting-matrix.md`

## Bottleneck Lanes (Canonical)
- `B1`: Scheduler queueing inflation (`scheduler_queueing`)
- `B2`: Reflow CPU tail latency (`logical_reflow`)
- `B3`: Render preparation spikes (`render_prep`)
- `B4`: Presentation jitter and transient invalid frames (`presentation`)
- `B5`: Lock and memory pressure coupling from runtime health telemetry

## Ranking Method
Each candidate receives five scores (`1..5`):
- `impact`: expected reduction against SLO lanes `M1/M2/M3`
- `confidence`: evidence quality from implemented probes/docs/tests
- `effort`: implementation complexity and cross-module churn
- `risk`: semantic/render regression risk
- `dependency`: blocker weight from upstream unfinished evidence/tasks

Computed values:
- `priority_score = (impact * confidence) / max(1, effort)`
- `execution_score = priority_score - (risk * 0.35) - (dependency * 0.25)`

Interpretation:
- Higher `execution_score` => earlier execution.
- Any item with `dependency >= 4` is prework-only until blockers clear.

## Dependency-Aware Ranked Interventions (Interim)

| Rank | Intervention | Lanes | Impact | Confidence | Effort | Risk | Dependency | Execution score | Downstream beads |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|
| 1 | Two-phase transaction + latest-intent wins + boundary cancellation | `B1`, `B4` | 5 | 4 | 3 | 3 | 2 | 5.45 | `wa-1u90p.2.2`, `wa-1u90p.2.3`, `wa-1u90p.4.1` |
| 2 | Adaptive buffer headroom/shrink in resize hot paths | `B2`, `B3`, `B5` | 4 | 4 | 2 | 2 | 2 | 7.30 | `wa-1u90p.3.2`, `wa-1u90p.3.5` |
| 3 | Viewport-first incremental reflow + cold backlog completion | `B2`, `B4` | 5 | 3 | 4 | 4 | 3 | 3.55 | `wa-1u90p.3.2`, `wa-1u90p.3.8`, `wa-1u90p.3.9` |
| 4 | Cursor snapshot retention controls + warning-driven degradation hooks | `B5` | 3 | 4 | 2 | 2 | 1 | 4.85 | `wa-1u90p.6.1`, `wa-1u90p.7.4` |
| 5 | Dirty-span-aware render diffing (row/span granularity) | `B3`, `B4` | 4 | 3 | 4 | 4 | 3 | 2.55 | `wa-1u90p.4.1`, `wa-1u90p.4.2` |
| 6 | Strategy selector for full diff vs dirty-only vs repaint under pressure | `B3`, `B5` | 3 | 3 | 3 | 3 | 3 | 2.40 | `wa-1u90p.4.7`, `wa-1u90p.7.4` |

Why rank changed:
- Transaction/control-plane intervention moved to the top because it now has an accepted ADR and concrete queue/coalescing implementation evidence in pane-path analysis docs.
- Buffer headroom/shrink remains a high-EV candidate due low effort and direct allocation-churn mitigation.
- Reflow algorithm work remains critical but still dependency-heavy due missing finalized lock/memory curves from `wa-1u90p.1.3`.

## Evidence Backing Each Lane
- `B1` (`scheduler_queueing`): explicitly modeled in timeline stage schema and per-event queue metrics (`ResizeQueueMetrics`).
- `B2` (`logical_reflow`): isolated stage with per-event duration and summary p95/p99 derivation path.
- `B3` (`render_prep`): explicit stage lane with flame-sample export.
- `B4` (`presentation`): explicit stage lane and direct coupling to stale-frame/artifact class budgets in SLO contract.
- `B5` (lock/memory): live runtime metrics and hard warning thresholds for storage lock wait/hold and cursor snapshot memory.

## What Is Unblocked Now vs Blocked

Unblocked prework/implementation guidance now:
1. Keep `wa-1u90p.2.*` control-plane slices aligned to ADR-0011 state invariants.
2. Continue low-risk lock-avoidance and queue/coalescing hardening in pane-level paths.
3. Add instrumentation-backed evidence capture templates to avoid ad-hoc profiling outputs.

Still blocked for final closeout of `wa-1u90p.1.5`:
1. Final lock contention + memory growth attribution from live runtime telemetry in `wa-1u90p.1.3`.
2. Artifact incidence trend lines mapped to `M3` budgets for repeated long-haul runs.

### Fresh quantitative baseline rollup (2026-02-14)

Source:

- `evidence/wa-1u90p.1.3/summaries/resize_baseline_timeline_rollup_2026-02-14.json`

This rollup now provides consolidated stage percentiles/maxima and queue-depth peaks across all canonical baseline fixtures:

| Scenario | Events | Queue max before | `logical_reflow.p95_ns` | `logical_reflow.max_ns` | `presentation.p95_ns` | `presentation.max_ns` |
|---|---:|---:|---:|---:|---:|---:|
| `resize_single_pane_scrollback` | 8 | 8 | 3,000 | 3,766,917 | 4,458 | 234,708 |
| `resize_multi_tab_storm` | 24 | 24 | 285,833 | 325,917 | 12,875 | 15,750 |
| `font_churn_multi_pane` | 24 | 24 | 190,834 | 216,084 | 8,792 | 10,708 |
| `mixed_scale_soak` | 28 | 28 | 530,000 | 671,167 | 38,875 | 118,250 |

Operational interpretation:

- `logical_reflow` remains the dominant latency lane (`B2`) in every scenario.
- `resize_single_pane_scrollback` carries the largest isolated spike (`3,766,917 ns`), indicating an outlier-heavy path.
- `mixed_scale_soak` has the heaviest sustained tail pressure (`logical_reflow.p95=530,000 ns`, highest multi-pane presentation tails), making it the best near-term gating workload for intervention A/B comparisons.

### Latest `wa-1u90p.1.3` intake (2026-02-14)
Primary report received:
- `docs/resize-lock-memory-profile-wa-1u90p.1.3.md`

Bundle received:
- `evidence/wa-1u90p.1.3/summaries/simulate_cli_parse.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_harness_build.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_harness_build_error_summary.txt`
- `evidence/wa-1u90p.1.3/summaries/simulate_cli_parse_2026-02-14.log`
- `evidence/wa-1u90p.1.3/summaries/simulation_resize_suite_no_run_2026-02-14.log`
- `evidence/wa-1u90p.1.3/summaries/resize_single_pane_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/resize_multi_tab_storm_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/font_churn_multi_pane_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/mixed_scale_soak_timeline.json`
- `evidence/wa-1u90p.1.3/summaries/resize_baseline_timeline_rollup_2026-02-14.json`
- `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_telemetry_refs.txt`
- `evidence/wa-1u90p.1.3/summaries/localpane_resize_telemetry_refs.txt`
- `evidence/wa-1u90p.1.3/summaries/docs_cross_refs.txt`

Historical blocker categories from earlier intake are now resolved:

- Baseline CLI replay mismatch (`generate_scrollback`) was caused by stale binary invocation and is cleared when running current source via `cargo run -p frankenterm -- simulate ...`.
- Harness compile blockers that previously prevented percentile extraction (`E0308`/`E0277`/`E0609`/`E0658` classes) are cleared in current checks.

Current blocker status (verified 2026-02-14):
- `CARGO_TARGET_DIR=target-violetdune cargo check --all-targets` succeeds.
- `CARGO_TARGET_DIR=target-violetdune cargo test -p frankenterm-core --test simulation_resize_suite -- --nocapture` succeeds (`4/4`).
- `CARGO_TARGET_DIR=target-violetdune cargo run -p frankenterm -- simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json` succeeds (includes `GenerateScrollback` events).

Implication: ranking confidence for `B1`/`B2`/`B3` is no longer blocked by replay/compile failures; remaining uncertainty is concentrated in live lock/memory growth curves (`B5`) and long-haul artifact incidence trends.

## Closure Artifact Contract (Must-Have)
- Per workload class (`R1..R4`) p50/p95/p99 for end-to-end resize interaction latency.
- Stage-level percentile summaries (`input_intent`, `scheduler_queueing`, `logical_reflow`, `render_prep`, `presentation`).
- Lock/memory telemetry summary with max/avg wait-hold metrics and contention counts.
- Artifact incidence report tied to critical/minor budgets.
- Final ranked table with confidence rationale and explicit go-order for downstream beads.

## Data Extraction Playbook (Deterministic)
```bash
# Timeline artifact envelope (scenario metadata + timeline + stage_summary + flame_samples)
cargo run -p frankenterm -- simulate run \
  fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml \
  --json --resize-timeline-json

# Resize timeline schema + stage coverage regression checks
cargo test -p frankenterm-core simulation_resize_suite -- --nocapture
cargo test -p frankenterm-core resize_timeline_summary_and_flame_samples_cover_all_stages -- --nocapture

# Runtime lock/memory warning threshold checks
cargo test -p frankenterm-core warning_threshold_fires -- --nocapture
```

## Revision Log
- 2026-02-13: Initial scaffold with scoring rubric and first-pass ranking.
- 2026-02-14: Interim dependency-aware ranking update with code-linked evidence map, clarified blocked/unblocked sequencing, and explicit closure artifact contract.
- 2026-02-14 (later): Incorporated fresh `wa-1u90p.1.3` artifact intake and explicit blocker taxonomy from profiling handoff.
- 2026-02-14 (latest): Added post-unblock verification artifacts showing simulation harness compile path is green; narrowed active blocker set to scenario replay schema mismatch.
- 2026-02-14 (latest+1): Added consolidated 4-scenario timeline rollup with queue-depth and stage-tail metrics; updated blocker model to focus on remaining live lock/memory percentile capture.
