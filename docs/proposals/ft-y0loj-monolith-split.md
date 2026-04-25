# Proposal: split `frankenterm-core` into sub-crates

**Bead:** [ft-y0loj](../../.beads/issues.jsonl) — `frankenterm-core is a 488-file / ~854K-line monolith`
**Status:** draft, not yet decomposed into child beads
**Related:** ft-7iof6 (runtime_compat reframe), ft-zoxxq (wezterm pseudo-boundary)

## Problem

`crates/frankenterm-core` is a single library with:

| metric                                  | count   |
|-----------------------------------------|---------|
| `.rs` files in `src/`                   | 488     |
| `mod` declarations in `lib.rs`          | 403     |
| feature flags in `Cargo.toml`           | 36      |
| approximate LOC                         | ~854 K  |

Every consumer (the GUI binary, the mux server binary, the fuzz crate, the
integration test crates, downstream tooling) compiles the full graph. The
`N×M` feature matrix is uncitable — there is no documented combination that
is known to be tested in CI, and `--no-default-features` plus any single
feature regularly fails to compile because internal modules silently rely on
sibling features being on.

This shows up in three concrete pain points the swarm has hit repeatedly:

1. **Cold compile time.** A clean build of any binary that touches core
   takes 3–5 minutes locally, longer under disk pressure.
2. **Feature-gate drift.** Modules added under one feature reach into
   sibling modules that turn out to require a different feature; the only
   discoverable way to learn this is `cargo check` with the offending
   combination. The pre-existing 5 compile errors that block fuzz-target
   verification under the `fuzz` feature are an instance of this class.
3. **Test-isolation cost.** A typo in `mcp_tools.rs` invalidates the cache
   for every other module's tests because `lib test` is one big crate.

The bead's framing — a 488-file monolith with no sub-crates — is accurate.
The fix is structural.

## Goals

- **G1.** Cut the cold-build cost for the GUI and fuzz consumers by ≥30 %
  by letting them depend on a strict subset of sub-crates.
- **G2.** Make the feature matrix tractable: at most one optional cargo
  feature per sub-crate, no cross-sub-crate feature unification.
- **G3.** Keep the public API of `frankenterm-core` unchanged for one
  release cycle: it becomes a façade crate that re-exports the sub-crate
  surface its current consumers depend on.
- **G4.** Land each sub-crate behind its own incremental commit, each
  shipping with the existing tests intact.

## Non-goals

- Rewriting any subsystem. This is a re-shelving exercise.
- Splitting the GUI crate, the codec crate, or the vendored `frankenterm/*`
  workspace crates — those are owned by separate panes (per AGENTS.md).
- Touching `runtime_compat` (covered by ft-7iof6) or the wezterm seam
  (covered by ft-zoxxq). Each is its own decomposition project.

## Proposed sub-crate layout

The 403 `mod` declarations cluster cleanly. The clusters below are
extraction candidates, ordered roughly from leaf (low fan-in from rest of
core) to hub (high fan-in). A "leaf" can be cut today; a "hub" needs its
leaves cut first.

### Tier 1 — leaves, extractable independently

| sub-crate                          | modules (count, examples)                                                       | existing feature gate     |
|------------------------------------|---------------------------------------------------------------------------------|---------------------------|
| `frankenterm-core-tantivy`         | `tantivy_ingest`, `tantivy_policy`, `tantivy_quality`, `tantivy_query`, `tantivy_reindex`, `recorder_lexical_*` (~7 modules) | `recorder-lexical`        |
| `frankenterm-core-ars`             | `ars_blast_radius`, `ars_compile`, `ars_drift`, `ars_evidence`, `ars_evolve`, `ars_explain`, `ars_federation`, `ars_fst`, `ars_generalize`, `ars_intercept`, `ars_replay`, `ars_secret_scan`, `ars_serialize`, `ars_symbolic_exec`, `ars_timeout` (15 modules) | none currently            |
| `frankenterm-core-fleet`           | `fleet_dashboard`, `fleet_launcher`, `fleet_memory_controller`, `fleet_scrollback_coordinator` (4 modules) | none currently            |
| `frankenterm-core-replay`          | `replay`, `replay_artifact_registry`, `replay_capture`, `replay_checkpoint`, `replay_ci_gate`, `replay_cli`, `replay_counterfactual`, `replay_decision_diff`, `replay_decision_graph`, `replay_fault_injection`, `replay_fixture_harvest`, `replay_guardrails`, `replay_guardrails_gate`, `replay_guide`, `replay_mcp`, `replay_merge`, `replay_performance`, `replay_post_incident`, `replay_provenance`, `replay_remediation`, `replay_report`, `replay_risk_scoring`, `replay_robot`, `replay_scenario_matrix`, `replay_shadow_rollout`, `replay_side_effect_barrier`, `replay_test_orchestrator`, `replay_usability_pilot` (28 modules) | none currently            |

These four sub-crates account for ~54 modules — **~13 %** of `lib.rs` —
and have light inbound dependence from the rest of core. Cutting them
first proves the pattern with minimal risk.

### Tier 2 — feature-gated subsystems

| sub-crate                          | modules (count, examples)                                                       | existing feature gate     |
|------------------------------------|---------------------------------------------------------------------------------|---------------------------|
| `frankenterm-core-mcp`             | `mcp`, `mcp_client`, `mcp_error`, `mcp_framework`, `mcp_tools`, `mcp_bridge`, `mcp_helpers`, etc. | `mcp`, `mcp-client`       |
| `frankenterm-core-connectors`      | `connector_*` (12 modules)                                                      | none currently            |
| `frankenterm-core-patterns`        | `patterns`, `pattern_trigger`, `scan_pipeline`                                  | none currently            |
| `frankenterm-core-policy`          | `policy`, `policy_audit_chain`, `policy_compliance`, `policy_decision_log`, `policy_diagnostics`, `policy_dsl`, `policy_metrics`, `policy_quarantine` (8 modules) | none currently            |
| `frankenterm-core-tx`              | `tx_execution`, `tx_idempotency`, `tx_observability`, `tx_plan_compiler` (4 modules) | `subprocess-bridge`       |
| `frankenterm-core-mission`         | `mission_agent_mail`, `mission_dispatch`, `mission_events`, `mission_loop` (4 modules) | `subprocess-bridge`       |

### Tier 3 — hubs, extracted last

| sub-crate                          | modules (count, examples)                                                       | rationale                              |
|------------------------------------|---------------------------------------------------------------------------------|----------------------------------------|
| `frankenterm-core-recorder`        | `recording`, `recorder_audit`, `recorder_export`, `recorder_invariants`, `recorder_migration`, `recorder_query`, `recorder_replay`, `recorder_retention`, `recorder_storage` | central data plane; many tier-1/2 importers |
| `frankenterm-core-storage`         | `storage`, `storage_targets`, `storage_telemetry`                               | SQLite + FTS5; recorder + tantivy + mission all import |
| `frankenterm-core-workflows`       | `workflows/*` (engine, runner, handlers, context, ~12 modules)                  | imports recorder, mcp, patterns, policy |
| `frankenterm-core-mux`             | `native_events`, `mux_*` integration                                            | bridges to vendored `mux/` crate; coupled to wezterm seam |

Tier-3 extraction is the cliff. We expect to need a follow-up proposal
that uses Tier-1/2 experience to choose between (a) leaving the hubs in
the `frankenterm-core` façade or (b) doing the trait-extraction work to
decouple them. Defer that decision.

## Phased plan

Each phase is one bead, one PR, one commit. No phase changes module
contents — only `Cargo.toml` files and `mod` declarations.

| phase | scope                                              | success signal                                                  |
|-------|----------------------------------------------------|-----------------------------------------------------------------|
| 0     | proposal lands (this doc)                          | doc reviewed; child beads filed (≤6, one per Tier-1/2 sub-crate) |
| 1     | extract `frankenterm-core-tantivy`                 | `cargo check -p frankenterm-core-tantivy` passes; existing tests run; `frankenterm-core` re-exports keep all consumer call sites green |
| 2     | extract `frankenterm-core-ars`                     | same |
| 3     | extract `frankenterm-core-fleet`                   | same |
| 4     | extract `frankenterm-core-replay`                  | same |
| 5     | measure: GUI + fuzz crate cold-build time          | report ≥15 % improvement (interim G1) |
| 6     | extract `frankenterm-core-patterns`                | fuzz crate can drop `frankenterm-core` dep entirely (uses only `-patterns` + `-recorder` for some targets) |
| 7     | extract `frankenterm-core-mcp`                     | `mcp` + `mcp-client` features migrate to the new crate's own features |
| 8     | extract `frankenterm-core-connectors`              | same |
| 9     | extract `frankenterm-core-policy`                  | same |
| 10    | extract `frankenterm-core-tx` + `-mission`         | `subprocess-bridge` feature splits cleanly |
| 11    | re-measure cold-build time                         | report total improvement vs. baseline (G1 acceptance) |
| 12    | document or revert tier-3 plan                     | either follow-up proposal or "park hubs in core" decision recorded as ADR |

Phase 0 is what this commit ships.

## Risks and how we'll know

| risk                                                                  | early-warning signal                                                  | mitigation                                                                     |
|-----------------------------------------------------------------------|-----------------------------------------------------------------------|--------------------------------------------------------------------------------|
| Sub-crate has hidden inbound dep we missed                            | tier-1 extraction PR fails to compile                                 | revert; refile bead with the discovered dependency edge                        |
| Cyclic dep between tier-1 sub-crates                                  | `cargo check` fails with E0432                                        | pull common types into a shared `frankenterm-core-types` crate                 |
| `#![forbid(unsafe_code)]` semantics drift across sub-crates           | grep audit                                                            | inherit the lint at workspace level via `lints.workspace = true`               |
| Build-time gain is ≤5 % despite all tier-1/2 cuts                     | phase-5 / phase-11 measurement                                        | stop at tier 2; reframe the proposal around feature-matrix sanity, not speed   |
| Façade re-export drift (a sub-crate adds public symbols not surfaced) | downstream `cargo check` after phase                                  | mechanical sweep: `cargo public-api` baseline before phase, diff after         |

## Open questions

1. **Test relocation.** Do unit tests move with their module, or do we
   keep them in a single `tests/` workspace crate? Tier-1 extraction will
   force this decision; the proposal recommends moving them with the
   module to keep the file:test mapping intact.
2. **Workspace inheritance.** The current `Cargo.toml` defines 36
   features at the core level. Each sub-crate inheriting via
   `[features] forward-mcp = ["frankenterm-core-mcp/foo"]` versus
   defining its own features — pick a convention before phase 1.
3. **GUI crate's expected sub-crate set.** The GUI today imports
   `frankenterm_core::*` widely. We need a static analysis pass before
   phase 1 to enumerate exactly which symbols it touches, so the façade
   re-export list is complete.

## Acceptance criteria for this proposal

- [ ] One reviewer signs off on the tier-1 sub-crate boundaries.
- [ ] Phase-0 child beads filed under `ft-y0loj` (one per tier-1/2
      sub-crate, plus a measurement bead for phase 5 and phase 11).
- [ ] Open questions 1–3 have a recorded answer (or are explicitly
      deferred to phase 1).

The next commit in this thread should be the child-bead creation pass.
