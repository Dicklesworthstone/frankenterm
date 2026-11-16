# Resize/Reflow No-Regression Compatibility Contract

Bead: `wa-1u90p.8.1`  
Parent track: `wa-1u90p.8` (Rollout, Compatibility, and Operator Experience)  
Downstream consumers: `wa-1u90p.8.2` (staged rollout), `wa-1u90p.8.6` (final go/no-go), `wa-1u90p.7.7` (alt-screen conformance e2e)

## Purpose

Resize/reflow performance work is only shippable if user-visible terminal semantics remain correct.
This document is the release compatibility contract for resize/reflow changes and is a hard gate for rollout advancement.

## Scope

This contract defines non-regression guarantees for:

- cursor behavior during and after resize
- wrapping/reflow semantics
- scrollback integrity
- alt-screen behavior
- interaction continuity (safe input and stable workflow behavior)
- lifecycle/presentation monotonicity

## Compatibility Invariants

Each invariant must stay true in candidate releases. A failing invariant blocks rollout progression.

| ID | Invariant | User-visible guarantee | Automated coverage (minimum) | Evidence source |
|---|---|---|---|---|
| `RC-CURSOR-001` | Cursor bounds + stable mapping | Cursor never renders out-of-bounds and does not jump to impossible coordinates during resize transitions | `crates/frankenterm-core/tests/proptest_resize_invariants.rs`, `crates/frankenterm-core/tests/resize_pipeline_integration.rs` | `docs/resize-performance-slos.md` |
| `RC-WRAP-001` | Deterministic wrapping/reflow | Rewrap decisions are deterministic for equivalent inputs and do not regress line-shaping semantics | `crates/frankenterm-core/tests/proptest_viewport_reflow_planner.rs`, `crates/frankenterm-core/tests/resize_pipeline_integration.rs` | `docs/resize-baseline-scenarios.md` |
| `RC-SCROLLBACK-001` | Scrollback integrity | Resize and font churn do not drop, reorder, or corrupt retained scrollback content | `crates/frankenterm-core/tests/proptest_restore_scrollback.rs`, `crates/frankenterm-core/tests/simulation_resize_suite.rs` | `docs/resize-baseline-scenarios.md` |
| `RC-ALTSCREEN-001` | Alt-screen correctness | Alt-screen enter/leave state transitions are detected reliably and represented as explicit boundary semantics | `crates/frankenterm-core/src/screen_state.rs`, `crates/frankenterm-core/src/ingest.rs` | `docs/resize-artifact-fault-model-wa-1u90p.4.1.md` |
| `RC-INTERACTION-001` | Interaction continuity + safety | Interactive workflows remain stable through resize events and unsafe sends are blocked when pane state is unsafe | `crates/frankenterm-core/src/workflows/mod.rs`, `crates/frankenterm-core/tests/resize_scheduler_state_machine_tests.rs` | `docs/resize-performance-slos.md` |
| `RC-LIFECYCLE-001` | Lifecycle monotonicity + non-stale commit | Superseded resize intents must not commit stale presentation; lifecycle ordering remains monotonic | `crates/frankenterm-core/tests/resize_invariant_contract.rs`, `crates/frankenterm-core/tests/proptest_resize_scheduler.rs`, `crates/frankenterm-core/tests/resize_scheduler_state_machine_tests.rs` | `docs/resize-artifact-fault-model-wa-1u90p.4.1.md` |

## Release Gate Rules

- All invariants above are mandatory in CI for resize/reflow rollout candidates.
- Any failure in `RC-ALTSCREEN-001` or `RC-INTERACTION-001` is auto-promoted to rollout blocker severity.
- Any failure in `RC-LIFECYCLE-001` is a no-go for release candidate promotion.

## Escalation Process

If any compatibility invariant fails:

1. Freeze rollout advancement in `wa-1u90p.8.2` immediately.
2. Record failing suite, seed, and artifacts in the bead thread and incident notes.
3. Classify severity:
   - critical: stale commit, unsafe interaction continuity break, or unrecoverable alt-screen semantic drift
   - high: deterministic reproducible user-visible semantic regression
   - medium: non-deterministic regression requiring repeated-run reproduction
4. Apply mitigation:
   - disable or rollback the offending optimization slice
   - rerun mapped suites for the affected invariant ID
5. Re-open go/no-go checklist evidence in `wa-1u90p.8.6` and require a clean rerun before promotion.

## Change Control

- Any resize/reflow change that affects user-visible semantics must update this contract if it changes invariant scope, evidence mapping, or escalation policy.
- Additive invariant IDs are allowed; removing an invariant requires explicit sign-off in `wa-1u90p.8.6`.
