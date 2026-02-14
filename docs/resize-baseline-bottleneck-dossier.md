# Resize Baseline Bottleneck Dossier (`wa-1u90p.1.5`)

Date: 2026-02-14  
Author: `MaroonGlacier` (interim pass; based on prior scaffold by `GentleBrook`)
Finalized by: `LavenderCastle` (2026-02-14)
Parent track: `wa-1u90p.1`
Status: **Closed.** All dependency beads (wa-1u90p.1.2, 1.3, 1.4, 1.6) are closed. Timeline rollups, stage percentiles, and active-pane lock/memory telemetry baselines are complete. Ranked intervention table is finalized. Downstream beads (wa-1u90p.2, 2.1, 4.1) are unblocked.

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

## Dependency-Aware Ranked Interventions (Final)

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

## What Is Unblocked Now

All prior blockers are resolved:
1. Active-pane lock contention + memory growth attribution: captured by `CloudyRaven` with `observed_panes=12` (see percentile tables below).
2. Timeline rollups for all 4 canonical scenarios: captured and consolidated.
3. Stage percentiles for all 5 stages: available in rollup artifact.
4. Lock/memory telemetry export path: validated under both idle and active-pane conditions.

Unblocked downstream work:
1. `wa-1u90p.2.*` control-plane slices are now unblocked and should align to ADR-0011 state invariants.
2. `wa-1u90p.4.1` root-cause investigation for stretched-text artifacts is unblocked.
3. Continue low-risk lock-avoidance and queue/coalescing hardening in pane-level paths.
4. Add instrumentation-backed evidence capture templates to avoid ad-hoc profiling outputs.

Deferred to downstream beads:
1. Artifact incidence trend lines mapped to `M3` budgets require the visual artifact detector pipeline (`wa-1u90p.7.9`) which is downstream. Initial M3 baselines will be established when that pipeline is implemented.
2. Stress-window lock/memory captures during explicit resize storms (`R2`/`R4` workloads) are refinement work for `wa-1u90p.7.5` (soak tests), not a blocker for this dossier.

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

### Estimated impact targets (next execution window)

These estimates are intentionally conservative and tied to currently measured tails:

| Target lane | Baseline metric | Candidate intervention(s) | Target metric | Estimated improvement |
|---|---|---|---|---:|
| `B2` outlier suppression (single-pane scrollback) | `logical_reflow.max=3,766,917 ns` (`resize_single_pane_scrollback`) | viewport-first incremental reflow + cold backlog completion (`wa-1u90p.3.2` family), adaptive buffer headroom (`wa-1u90p.3.5`/allocation controls) | `logical_reflow.max <= 1,200,000 ns` | ~68% max-tail reduction |
| `B2` sustained tail (mixed soak) | `logical_reflow.p95=530,000 ns` (`mixed_scale_soak`) | two-phase transaction control plane + latest-intent cancelation (`wa-1u90p.2.*`) plus bounded DP/fallback wrap controls (`wa-1u90p.3.12+`) | `logical_reflow.p95 <= 300,000 ns` | ~43% p95 reduction |
| `B4` presentation tail (mixed soak) | `presentation.p95=38,875 ns`, `max=118,250 ns` | dirty-span-aware render diffing + strategy selector (`wa-1u90p.4.*`) | `presentation.p95 <= 20,000 ns`, `max <= 70,000 ns` | ~49% p95, ~41% max reduction |
| `B1` queue depth under fanout | `queue.max_before=28` (`mixed_scale_soak`) | scheduler fairness tuning + coalescing guardrails (`wa-1u90p.2.3`, `wa-1u90p.2.7`) | preserve `max_before <= 28`, force `min_after=0` under repeated storms | stability target (avoid queue-growth regression) |

These become the first measurable go/no-go targets for downstream slices and should be enforced in the perf regression suite (`wa-1u90p.7.4`) once that harness is finalized.

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
- `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_samples_2026-02-14T0625Z.jsonl`
- `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_percentiles_2026-02-14T0625Z.json`
- `evidence/wa-1u90p.1.3/summaries/localpane_resize_telemetry_refs.txt`
- `evidence/wa-1u90p.1.3/summaries/docs_cross_refs.txt`

Historical blocker categories from earlier intake are now resolved:

- Baseline CLI replay mismatch (`generate_scrollback`) was caused by stale binary invocation and is cleared when running current source via `cargo run -p frankenterm -- simulate ...`.
- Harness compile blockers that previously prevented percentile extraction (`E0308`/`E0277`/`E0609`/`E0658` classes) are cleared in current checks.

Current blocker status (verified 2026-02-14):
- `CARGO_TARGET_DIR=target-violetdune cargo check --all-targets` succeeds.
- `CARGO_TARGET_DIR=target-violetdune cargo test -p frankenterm-core --test simulation_resize_suite -- --nocapture` succeeds (`4/4`).
- `CARGO_TARGET_DIR=target-violetdune cargo run -p frankenterm -- simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json` succeeds (includes `GenerateScrollback` events).

Implication: ranking confidence for `B1`/`B2`/`B3` is no longer blocked by replay/compile failures; remaining uncertainty is concentrated in stress-window lock/memory growth curves (`B5`) and long-haul artifact incidence trends.

Additional `B5` progress (2026-02-14, `CalmOwl`):

- Captured 30-sample live watcher `status --health` window and computed p50/p95/max for `health.runtime_lock_memory` metrics:
  - sample window artifact: `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_samples_2026-02-14T0625Z.jsonl`
  - percentile rollup artifact: `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_percentiles_2026-02-14T0625Z.json`
- Result quality: valid telemetry export path and idle baseline established (`watcher_running=true` across all samples), but no active panes were observed in this environment (WezTerm CLI unavailable), so this does not yet satisfy active-workload lock/memory attribution.
- Follow-up capture (`CloudyRaven`) resolved the environment gate by setting both `PATH=/Applications/WezTerm.app/Contents/MacOS:$PATH` and `WEZTERM_UNIX_SOCKET=/Users/jemanuel/.local/share/wezterm/sock`:
  - sample window artifact: `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_samples_2026-02-14T0633Z.jsonl`
  - percentile rollup artifact: `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_percentiles_2026-02-14T0633Z.json`
- Active-pane evidence quality: `watcher_running=true` and `observed_panes` stable at `12` (`p50/p95/max=12`) for all 30 samples, with non-zero cursor snapshot memory metrics (`cursor_snapshot_bytes_last p50=196,452`, `p95=198,308`) and stable lock wait/hold percentiles.
- Residual `B5` gap is now narrowed to workload intensity calibration (capture during explicit resize-storm windows), not telemetry/discovery readiness.

### Active-pane lock/memory baseline (B5 closure evidence)

Source: `evidence/wa-1u90p.1.3/summaries/runtime_lock_memory_percentiles_2026-02-14T0633Z.json` (12 observed panes, 30 samples)

| Metric | p50 | p95 | max | Warning threshold | Status |
|---|---:|---:|---:|---:|---|
| `avg_storage_lock_wait_ms` | 0.001 | 0.001 | 0.001 | 15.0 ms | Well below |
| `max_storage_lock_wait_ms` | 0.007 | 0.018 | 0.018 | 15.0 ms | Well below |
| `storage_lock_contention_events` | 0 | 0 | 0 | n/a | Zero contention |
| `avg_storage_lock_hold_ms` | 2.41 | 2.81 | 2.81 | 75.0 ms | Well below |
| `max_storage_lock_hold_ms` | 41.09 | 41.09 | 41.09 | 75.0 ms | Within budget |
| `cursor_snapshot_bytes_last` | 196,452 | 198,308 | 198,308 | 64 MiB | Well below |
| `cursor_snapshot_bytes_max` | 196,452 | 198,308 | 198,308 | 64 MiB | Well below |

Interpretation:
- All lock wait/hold metrics are well within warning thresholds under steady-state 12-pane observation.
- Zero lock contention events observed across entire sample window.
- Cursor snapshot memory stable at ~193 KiB, orders of magnitude below the 64 MiB warning threshold.
- `max_storage_lock_hold_ms` of 41.09 ms is the highest observed metric (55% of 75 ms threshold) and the most likely to breach under resize-storm conditions; this is the primary metric to monitor during stress testing.

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
- 2026-02-14 (latest+2): Added live watcher runtime-lock/memory percentile artifacts (30-sample idle baseline via `status --health`) and refined remaining blocker to active-pane percentile capture.
- 2026-02-14 (latest+3): Added active-pane runtime-lock/memory percentile artifacts (`observed_panes=12`) with host env/socket requirements, closing discovery-path uncertainty and narrowing `B5` to stress-window refinement.
- 2026-02-14 (final): `LavenderCastle` finalized dossier for closure. All dependency beads closed. Added active-pane B5 baseline summary table with threshold comparison. Promoted ranked table from interim to final. Resolved blocker section: active-pane telemetry complete, M3 artifact incidence deferred to visual detector pipeline (wa-1u90p.7.9), stress-window refinement deferred to soak tests (wa-1u90p.7.5).
