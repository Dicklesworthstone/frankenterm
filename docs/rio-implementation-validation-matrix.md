# Rio Implementation Validation Matrix

Bead: `ft-34sko.8`  
Inputs: `docs/rio-analysis-synthesis.md`, `evidence/rio/*.md`  
Scope: convert Rio-derived recommendations into execution-ready implementation/validation contracts.

## Contract Rules (Mandatory for Every Mapped Implementation Bead)

1. Unit tests: happy path + edge cases + failure behavior.
2. Integration tests: cross-module behavior with realistic boundaries.
3. E2E script: deterministic fixture input and machine-readable assertions.
4. Structured JSONL logs: `run_id`, `scenario_id`, `pane_id`, `window_id`, `phase`, `decision`, `elapsed_ms`, `error_code`, `outcome`.
5. Artifact paths: each scenario must write JSONL + summary artifacts under `e2e-artifacts/rio/<scenario>/<run_id>/`.
6. Quality gates: deterministic `cargo` commands and expected outcomes documented in bead notes/comments.

## Recommendation -> Implementation Mapping

| Rec | Rio-derived recommendation | Implementation beads | Bead status | Required tests | Required e2e + fixtures | Required artifacts |
|---|---|---|---|---|---|---|
| R1 | Canonical wakeup/coalescing contract across ingest -> detect -> render | `ft-1u90p.7` (validation), `ft-1u90p.5` (historical impl) | Open + Closed | Unit: wakeup dedupe/coalesce semantics; Integration: ingest->eventbus->render trigger ordering | `tests/e2e/rio/test_wakeup_coalescing.sh` using `fixtures/rio/wakeup_coalescing` | `e2e-artifacts/rio/wakeup_coalescing/<run_id>/events.jsonl`, `summary.json` |
| R2 | Two-source damage model merge (terminal damage + UI damage) | `ft-1u90p.7` (validation), `ft-1u90p.4` (historical impl) | Open + Closed | Unit: damage merge precedence and full-fallback; Integration: partial-present correctness during resize | `tests/e2e/rio/test_damage_merge_partial_present.sh` using `fixtures/rio/damage_merge` | `e2e-artifacts/rio/damage_merge/<run_id>/damage_trace.jsonl`, `frame_diff_summary.json` |
| R3 | Sync-update guardrails + adaptive batch thresholds by pane activity | `ft-283h4.4` (active impl), `ft-1u90p.7` (validation), `ft-1u90p.5` (historical impl) | Open + Open + Closed | Unit: sync timeout/cap behavior; Integration: activity-tier batch switching under load | `tests/e2e/rio/test_sync_update_guardrails.sh` using `fixtures/rio/sync_update_batches` | `e2e-artifacts/rio/sync_update_guardrails/<run_id>/batch_metrics.jsonl`, `timeouts.json` |
| R4 | Unified memory budget controller (scrollback/cache/queue) | `ft-1u90p.7` (validation), `ft-1u90p.6` (historical impl), `ft-1u90p.5` (historical impl) | Open + Closed + Closed | Unit: budget transitions (`normal/constrained/emergency`); Integration: pressure -> degradation ladder | `tests/e2e/rio/test_memory_budget_degradation.sh` using `fixtures/rio/memory_pressure` | `e2e-artifacts/rio/memory_budget/<run_id>/budget_transitions.jsonl`, `rss_profile.json` |
| R5 | Pane-churn benchmark matrix with wakeup-to-frame SLOs | `ft-1u90p.7` (active), `ft-1u90p.7.9` (historical impl) | Open + Closed | Benchmarks: p50/p95/p99 wakeup->frame; Integration: mixed interactive + bulk panes | `tests/e2e/rio/test_pane_churn_matrix.sh` using `fixtures/rio/pane_churn` | `e2e-artifacts/rio/pane_churn/<run_id>/latency_histograms.json`, `timeline.jsonl` |
| R6 | Frame pacing policy tiers (latency/balanced/efficiency) | `ft-1u90p.8` (rollout/policy), `ft-1u90p.4` (historical impl) | Open + Closed | Unit: policy selection + fallback; Integration: pacing mode switch under monitor/occlusion transitions | `tests/e2e/rio/test_frame_pacing_policy_tiers.sh` using `fixtures/rio/frame_pacing` | `e2e-artifacts/rio/frame_pacing/<run_id>/pacing_decisions.jsonl`, `missed_frame_report.json` |
| R7 | Effective-config introspection + strict validation mode | `ft-1u90p.8` (active), `ft-vv3h` (historical impl), `ft-x4bt` (historical impl) | Open + Closed + Closed | Unit: precedence/source attribution; Integration: invalid/unknown/platform-mismatch config handling | `tests/e2e/rio/test_effective_config_resolve.sh` using `fixtures/rio/config_resolve` | `e2e-artifacts/rio/config_resolve/<run_id>/resolved_config.json`, `validation_events.jsonl` |

## Legacy Rio Code Anchors (2026-02-19 Verification Pass)

These anchors were verified directly against `legacy_rio/rio` to make downstream implementation beads less ambiguous.

| Rec | Verified legacy Rio anchors | Why it matters for FrankenTerm implementation beads |
|---|---|---|
| R1 | `legacy_rio/rio/rio-backend/src/performer/mod.rs:218` (Wakeup emission after non-sync parse), `legacy_rio/rio/frontends/rioterm/src/application.rs:304` (Wakeup handling + redraw scheduling), `legacy_rio/rio/frontends/rioterm/src/scheduler.rs:61` (timer dispatch) | Defines the concrete wakeup/coalescing path to preserve when implementing ingest->detect->render coalescing contracts. |
| R2 | `legacy_rio/rio/frontends/rioterm/src/context/renderable.rs:98` (`PendingUpdate` dirty/UI damage merge), `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:559` (`peek_damage_event`), `legacy_rio/rio/frontends/rioterm/src/renderer/mod.rs:884` (terminal+UI damage merge at render) | Confirms two-source damage merge semantics and where full/partial/cursor-only fallback is decided. |
| R3 | `legacy_rio/rio/rio-backend/src/performer/mod.rs:32` (`READ_BUFFER_SIZE`, `MAX_LOCKED_READ`), `legacy_rio/rio/rio-backend/src/performer/mod.rs:213` (lock-duration guard), `legacy_rio/rio/rio-backend/src/performer/mod.rs:335` (sync timeout handling) | Establishes concrete batching/guardrail knobs and timeout behavior for adaptive sync/update bead work. |
| R4 | `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:448` (fixed scrollback allocation), `legacy_rio/rio/rio-backend/src/crosswords/mod.rs:588` (damage reset lifecycle) | Shows Rio baseline lacks a unified memory-budget controller; FrankenTerm beads must add explicit budget governance rather than assuming it exists upstream. |
| R5 | `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:511` (`wait_until` frame timing), `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:569` (`update_vblank_interval`), `legacy_rio/rio/frontends/rioterm/src/application.rs:1420` (redraw continuation rules) | Provides the frame-timing and redraw hot path needed for pane-churn SLO instrumentation and benchmark wiring. |
| R6 | `legacy_rio/rio/frontends/rioterm/src/screen/mod.rs:140` (performance/backend policy selection), `legacy_rio/rio/frontends/rioterm/src/application.rs:220` (unfocused/occluded render gating), `legacy_rio/rio/frontends/rioterm/src/router/mod.rs:61` (platform-specific redraw scheduling) | Anchors policy-tier behavior to concrete render gating and pacing decisions for rollout/ops beads. |
| R7 | `legacy_rio/rio/rio-backend/src/config/mod.rs:378` (`try_load` + error surfaces), `legacy_rio/rio/rio-backend/src/config/mod.rs:458` (platform override merge), `legacy_rio/rio/frontends/rioterm/src/watcher.rs:35` (config change events), `legacy_rio/rio/frontends/rioterm/src/application.rs:357` (debounced config reload apply) | Defines effective-config precedence and live reload execution points required for strict validation/introspection contracts. |

## Deterministic Validation Command Contracts

All implementation beads mapped above must include these command classes in notes/comments:

1. Unit/integration:
```bash
rch exec -- cargo test --workspace --all-targets
```
2. Lint:
```bash
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
```
3. Formatting:
```bash
cargo fmt --check
```
4. Scenario script invocation shape (per matrix row):
```bash
tests/e2e/rio/<scenario_script>.sh --fixtures fixtures/rio/<scenario_fixture> --run-id <run_id>
```

## Logging Schema Additions (Scenario-Specific Fields)

Beyond the base JSONL contract, each scenario must emit:

- `queue_depth`, `coalesced_count`, `wakeup_to_frame_ms` for R1/R5.
- `damage_scope`, `fallback_to_full`, `dirty_regions` for R2.
- `sync_hold_bytes`, `batch_size`, `activity_tier`, `guardrail_triggered` for R3.
- `memory_tier`, `scrollback_bytes`, `cache_bytes`, `queue_bytes` for R4.
- `p50_ms`, `p95_ms`, `p99_ms`, `pane_count`, `event_rate` for R5/R6.
- `config_source`, `override_path`, `effective_value_hash`, `redacted_fields` for R7.

## Quality-Gate Completion Criteria for `ft-34sko.8`

`ft-34sko.8` is complete when:

1. Every recommendation in `docs/rio-analysis-synthesis.md` is mapped in this matrix.
2. Each mapped recommendation has at least one concrete open implementation/validation bead.
3. Every mapped bead has explicit unit/integration/e2e/logging/artifact contract text (either in acceptance criteria or comments).
4. Script path + fixture path + artifact path are deterministic and unambiguous.
