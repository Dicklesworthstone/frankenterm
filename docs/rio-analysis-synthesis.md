# Rio Analysis Synthesis — Prioritized FrankenTerm Adoption Roadmap

Bead: `ft-34sko.7`

This report synthesizes all Rio mining outputs into a concrete adoption roadmap for FrankenTerm, and explicitly compares the result with the existing Ghostty and Zellij syntheses.

Inputs (Rio):
- `evidence/rio/INVENTORY.md` (`ft-34sko.1`)
- `evidence/rio/rendering-pipeline.md` (`ft-34sko.2`)
- `evidence/rio/terminal-core.md` (`ft-34sko.3`)
- `evidence/rio/runtime-event-loop.md` (`ft-34sko.4`)
- `evidence/rio/config-platform.md` (`ft-34sko.5`)
- `evidence/rio/performance-analysis.md` (`ft-34sko.6`)

Cross-comparison inputs:
- `docs/ghostty-analysis-synthesis.md` (`ft-3bja.5`)
- `docs/zellij-analysis-synthesis.md` (`ft-2bai5`)

## Executive Summary

Rio’s highest-leverage contribution is not “new terminal features,” but a coherent runtime shape:
- parser/terminal mutation is decoupled from rendering via wakeup events,
- render work is damage-aware and frame-paced,
- parser and cache hot paths are bounded and explicit,
- platform/config layering is field-merge based and operationally practical.

The best FrankenTerm strategy is a hybrid:
- keep Ghostty-grade coalescing discipline,
- keep Zellij-grade operability/policy/session rigor,
- add Rio-grade damage + frame-pacing + parser-guardrail mechanics.

## Prioritized Recommendations (Actionable, Mapped)

| Rank | Recommendation | Effort | Risk | Primary bead mapping |
|---|---|---:|---:|---|
| 1 | Canonical wakeup/coalescing contract across ingest -> detect -> render | M | M | `ft-1u90p.5`, `ft-1u90p.7` |
| 2 | Two-source damage model (terminal damage + UI damage) with partial present | M | M | `ft-1u90p.4` |
| 3 | Sync-update guardrails + adaptive batch thresholds by pane activity | M | M | `ft-1u90p.5`, `ft-283h4.4` |
| 4 | Memory budget controller for scrollback + glyph/image caches + queue buffers | M | H | `ft-1u90p.5`, `ft-1u90p.6` |
| 5 | Pane-churn benchmark matrix with p95/p99 wakeup-to-frame SLOs | M | L | `ft-1u90p.7`, `ft-1u90p.7.9` |
| 6 | Frame pacing policy: monitor-aware scheduling + occlusion gating + explicit fallback modes | S-M | L | `ft-1u90p.4`, `ft-1u90p.8` |
| 7 | Effective-config introspection (source-of-truth per field + strict validation mode) | S | L | `ft-1u90p.8`, `ft-vv3h`, `ft-x4bt` |

## 1) Quick Wins (Low Effort, High Value)

### 1.1 Standardize wakeup coalescing and queue observability
Rio pattern:
- backend emits wakeups (`RioEvent::Wakeup`) instead of direct draw,
- scheduler avoids duplicate render timer topics,
- frame work is naturally damped by pacing.

FrankenTerm implementation sketch:
- enforce one coalescing contract for all high-rate event producers,
- expose queue depth, coalesced-drop count, and wakeup-to-frame latency metrics,
- require every high-rate queue to declare overflow behavior (`merge`, `drop-oldest`, `defer`).

Likely risk:
- medium; risk is over-coalescing and hiding causal signals if not instrumented.

Mapped beads:
- `ft-1u90p.5`, `ft-1u90p.7`

### 1.2 Adopt Rio-style two-source damage merge
Rio pattern:
- terminal-core emits `Full/Partial/CursorOnly` damage,
- frontend independently tracks UI damage, then merges both before render.

FrankenTerm implementation sketch:
- keep model damage and overlay damage as separate streams,
- merge at one render boundary,
- preserve “fallback-to-full” correctness behavior when damage certainty is low.

Likely risk:
- medium; partial updates can regress correctness without strong fixtures.

Mapped beads:
- `ft-1u90p.4`

### 1.3 Add strict effective-config visibility
Rio pattern:
- config layering is clear in code (default/file/platform/CLI/env), but not strongly surfaced as a single machine-readable resolution artifact.

FrankenTerm implementation sketch:
- `ft config resolve --format json` returning value + source for each field,
- strict mode that fails on invalid/unknown/platform-mismatched config keys,
- compatibility summary for operator/debug workflows.

Likely risk:
- low.

Mapped beads:
- `ft-1u90p.8`, `ft-vv3h`, `ft-x4bt`

## 2) Strategic Improvements (Medium Effort, High Leverage)

### 2.1 Bounded parser envelope: synchronized updates + adaptive batching
Rio pattern:
- synchronized-update timeout and buffer cap are explicit,
- parser batching threshold and shrink policy are explicit.

FrankenTerm implementation sketch:
- keep hard sync bounds,
- tune batch threshold by pane role (interactive vs bulk-output),
- add starvation guard for unfair lock fast paths.

Likely blockers:
- proving this does not regress latency tails on interactive panes.

Mapped beads:
- `ft-1u90p.5`, `ft-283h4.4`

### 2.2 Unified memory budgeting across terminal/runtime/render caches
Rio pattern:
- bounded char/font/image caches,
- explicit history depth and clear-history path,
- bounded parser transient buffers.

FrankenTerm implementation sketch:
- introduce global memory envelope with sub-budgets:
  - scrollback,
  - render caches,
  - transient ingest queues.
- tie policy to pressure states (`normal`, `constrained`, `emergency`) with deterministic degradation ladder.

Likely blockers:
- tuning policy without harming perceived quality.

Mapped beads:
- `ft-1u90p.5`, `ft-1u90p.6`

### 2.3 Frame scheduler hardening with explicit pacing policy tiers
Rio pattern:
- monitor-derived vblank interval,
- `wait_until` pacing,
- occlusion-aware render suppression.

FrankenTerm implementation sketch:
- scheduling policy per platform/workload (`latency`, `balanced`, `efficiency`),
- explicit fallback path for missed frame deadlines,
- attach pacing telemetry to go/no-go rollout dashboards.

Likely blockers:
- platform-specific behavior drift under non-default compositors.

Mapped beads:
- `ft-1u90p.4`, `ft-1u90p.8`

### 2.4 End-to-end pane-churn performance test matrix
Rio gap discovered:
- strong microbenches exist, but no full pipeline pane-churn benchmark harness.

FrankenTerm implementation sketch:
- add end-to-end benchmark matrix:
  - high pane count,
  - rapid resize churn,
  - mixed interactive + bulk output,
  - memory-pressure cycles.
- enforce SLO gates on p95/p99 wakeup-to-frame and queue lag.

Likely blockers:
- deterministic benchmark reproducibility in CI.

Mapped beads:
- `ft-1u90p.7`, `ft-1u90p.7.9`

## 3) Future Considerations (Defer With Rationale)

### 3.1 Per-pane IO thread vs shared-reactor model experiment
Rio today:
- optional performer thread per context.

Why defer:
- FrankenTerm currently has higher-priority correctness and observability work in active tracks; concurrency architecture changes should be benchmark-driven and phased.

Revisit when:
- `ft-1u90p.7` has stable churn benchmarks and `ft-283h4.4` IO pipeline data is available.

### 3.2 Atlas lifecycle optimization (compaction/segregation)
Rio today:
- dual mask/color atlases with dirty upload and full clear on font-change events.

Why defer:
- useful, but second-order until frame/timing/memory telemetry proves atlas pressure as a dominant bottleneck.

Revisit when:
- cache-pressure observability from strategic items is live.

### 3.3 Rio-style config ergonomics beyond core runtime tracks
Why defer:
- operationally useful, but less urgent than resize/reflow correctness and throughput tracks.

Revisit when:
- `ft-1u90p` rollout stabilization reaches evidence-complete state.

## 4) Cross-Comparison: Rio vs Ghostty vs Zellij

### 4.1 Convergent patterns across all three (high-confidence bets)

1. **Bounded, coalesced event propagation beats per-event fanout**
- Ghostty: explicit coalescing and mailbox drains.
- Zellij: bounded queues with explicit degradation semantics.
- Rio: wakeup-based render triggering with scheduler dedupe.

2. **Performance depends on explicit budget boundaries**
- Ghostty: byte-budgeted memory discipline.
- Zellij: bounded queues and pragmatic backpressure.
- Rio: bounded parser/caches/frame pacing defaults.

3. **Operational clarity matters as much as raw speed**
- Ghostty/Zellij syntheses emphasize explicit contracts and observability.
- Rio highlights where this should attach in the render/event pipeline.

### 4.2 Divergent strategies requiring FrankenTerm-specific tradeoffs

1. **Hot-path concurrency model**
- Ghostty: tight coalesced hot path.
- Rio: per-context performer thread + locking/pacing.
- Zellij: subsystem message-bus model.

Decision for FrankenTerm:
- maintain coalesced hot path semantics while keeping subsystem boundaries auditable.

2. **Output representation and rendering boundary**
- Zellij favors frame-oriented server output.
- Ghostty and Rio focus on internal dirty/delta semantics.

Decision for FrankenTerm:
- keep structured pane-addressed deltas for robot/search workflows, and only materialize frame-level views where needed.

3. **Config/ops philosophy**
- Zellij is protocol/compat heavy.
- Rio is practical platform-merge and defaults focused.
- Ghostty synthesis emphasizes hot-path behavior more than operator config UX.

Decision for FrankenTerm:
- combine protocol compatibility rigor with Rio-like effective-config transparency.

### 4.3 Rio-unique innovations (no direct counterpart in prior syntheses)

1. **Synchronized-update safeguards with hard timeout and cap**
2. **Two-source damage merge (terminal + UI) as first-class render contract**
3. **Monitor-aware `wait_until` pacing integrated with route scheduler**
4. **Dual atlas + unified text-run cache architecture wired to dirty uploads**

These are the highest-value Rio-specific additions to the existing Ghostty/Zellij-informed roadmap.

## 5) Execution Order

1. Ship quick wins first (coalescing contract, damage merge, config introspection).
2. Land strategic observability and budget control in parallel with active resize/reflow tracks.
3. Use newly added performance evidence to decide on concurrency/atlas future work.

## 6) Implementation Tracking (No Untracked Recommendations)

All selected recommendations (`R1`..`R7`) are mapped to concrete FrankenTerm beads, with no untracked implementation items:

| Rec | Recommendation (short) | Implementation beads |
|---|---|---|
| R1 | Wakeup coalescing contract | `ft-1u90p.5`, `ft-1u90p.7` |
| R2 | Two-source damage merge | `ft-1u90p.4`, `ft-1u90p.7` |
| R3 | Sync guardrails + adaptive batching | `ft-1u90p.5`, `ft-283h4.4`, `ft-1u90p.7` |
| R4 | Unified memory budget controller | `ft-1u90p.5`, `ft-1u90p.6`, `ft-1u90p.7` |
| R5 | Pane-churn SLO matrix | `ft-1u90p.7`, `ft-1u90p.7.9` |
| R6 | Frame pacing policy tiers | `ft-1u90p.4`, `ft-1u90p.8` |
| R7 | Effective-config introspection + strict mode | `ft-1u90p.8`, `ft-vv3h`, `ft-x4bt` |

Contract source of truth for downstream execution details: `docs/rio-implementation-validation-matrix.md` (`ft-34sko.8`).

## 7) Consolidated Validation Matrix (Unit/Integration/E2E/Logs/Artifacts/Quality Gates)

Every mapped implementation bead must carry the following contract elements: unit tests, integration tests, deterministic e2e script + fixture path, structured logs, artifact outputs, and quality gates.

| Rec | Unit tests | Integration tests | E2E script path | Fixture directory | Artifact path |
|---|---|---|---|---|---|
| R1 | Wakeup dedupe/coalesce semantics | Ingest -> eventbus -> render ordering | `tests/e2e/rio/test_wakeup_coalescing.sh` | `fixtures/rio/wakeup_coalescing` | `e2e-artifacts/rio/wakeup_coalescing/<run_id>/events.jsonl`, `summary.json` |
| R2 | Damage merge precedence + full fallback | Partial present correctness under resize | `tests/e2e/rio/test_damage_merge_partial_present.sh` | `fixtures/rio/damage_merge` | `e2e-artifacts/rio/damage_merge/<run_id>/damage_trace.jsonl`, `frame_diff_summary.json` |
| R3 | Sync timeout/cap + batch tier transitions | Guardrail-triggered fallback ordering | `tests/e2e/rio/test_sync_update_guardrails.sh` | `fixtures/rio/sync_update_batches` | `e2e-artifacts/rio/sync_update_guardrails/<run_id>/batch_metrics.jsonl`, `timeouts.json` |
| R4 | Budget state transitions (`normal/constrained/emergency`) | Pressure-driven degradation ladder behavior | `tests/e2e/rio/test_memory_budget_degradation.sh` | `fixtures/rio/memory_pressure` | `e2e-artifacts/rio/memory_budget/<run_id>/budget_transitions.jsonl`, `rss_profile.json` |
| R5 | Latency histogram math + SLO threshold evaluation | Mixed interactive + bulk pane contention | `tests/e2e/rio/test_pane_churn_matrix.sh` | `fixtures/rio/pane_churn` | `e2e-artifacts/rio/pane_churn/<run_id>/latency_histograms.json`, `timeline.jsonl` |
| R6 | Pacing policy selection + fallback | Mode switch across refresh/occlusion transitions | `tests/e2e/rio/test_frame_pacing_policy_tiers.sh` | `fixtures/rio/frame_pacing` | `e2e-artifacts/rio/frame_pacing/<run_id>/pacing_decisions.jsonl`, `missed_frame_report.json` |
| R7 | Config precedence/source attribution + strict validation | Invalid/unknown/platform-mismatch config handling | `tests/e2e/rio/test_effective_config_resolve.sh` | `fixtures/rio/config_resolve` | `e2e-artifacts/rio/config_resolve/<run_id>/resolved_config.json`, `validation_events.jsonl` |

### Structured Logging Fields (Mandatory)

Base JSONL fields for all scenarios:
- `run_id`, `scenario_id`, `pane_id`, `window_id`, `phase`, `decision`, `elapsed_ms`, `error_code`, `outcome`

Scenario-specific required fields:
- R1/R5: `queue_depth`, `coalesced_count`, `wakeup_to_frame_ms`
- R2: `damage_scope`, `fallback_to_full`, `dirty_regions`
- R3: `sync_hold_bytes`, `batch_size`, `activity_tier`, `guardrail_triggered`
- R4: `memory_tier`, `scrollback_bytes`, `cache_bytes`, `queue_bytes`
- R5/R6: `p50_ms`, `p95_ms`, `p99_ms`, `pane_count`, `event_rate`
- R7: `config_source`, `override_path`, `effective_value_hash`, `redacted_fields`

### Quality Gates (Mandatory)

For each mapped implementation bead, downstream agents must run:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- cargo test --workspace --all-targets
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
tests/e2e/rio/<scenario_script>.sh --fixtures fixtures/rio/<scenario_fixture> --run-id <run_id>
```

## 8) Acceptance Checklist (`ft-34sko.7`)

- All Rio analysis beads synthesized: yes (`ft-34sko.1`..`ft-34sko.6` inputs included).
- >=5 actionable recommendations with effort/risk: yes (7 listed).
- Recommendations mapped to existing/new beads: yes (mapping table + per-item mappings).
- Explicit side-by-side Ghostty/Zellij comparison: yes (Section 4).
- No selected recommendation untracked: yes (Section 6, R1..R7 all mapped).
- Consolidated validation matrix with script/fixture/artifact paths: yes (Section 7).
