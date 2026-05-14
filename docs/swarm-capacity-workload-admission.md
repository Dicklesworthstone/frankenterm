# Swarm Capacity Workload Admission

Status: `ft-b94bx.2` workload-class admission contract for high-core swarm
capacity planning.

This document defines the dry-run model that composes workload class,
context-horizon risk, blocker-radar claimability, herd-wave pressure, and
resource-pressure evidence into one conservative admission decision. The model
does not execute pane, workflow, or scheduler side effects.

Rust contract: `crates/frankenterm-core/src/runtime_telemetry.rs::SwarmCapacityWorkloadAdmissionPlan`

Fixture: `crates/frankenterm-core/tests/fixtures/swarm_capacity_workload_admission/examples.json`

E2E smoke: `tests/e2e/test_swarm_capacity_workload_admission.sh`

## Workload Classes

| Class | Existing workload surface | Existing work class | Fresh green action | Stale or unavailable evidence | Default units |
| --- | --- | --- | --- | --- | --- |
| `coding` | `backpressure_escalation` | `claimed_agent_task` | `admit` | `defer` | 2 |
| `reviewing` | `backpressure_escalation` | `claimed_agent_task` | `admit` | `defer` | 1 |
| `building` | `capacity_governor` | `claimed_agent_task` | `admit` | `defer` | 8 |
| `testing` | `capacity_governor` | `claimed_agent_task` | `admit` | `defer` | 6 |
| `idle` | `idle_observation` | `optional_diagnostics` | `admit` | `defer` | 1 |
| `blocked` | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` | 1 |
| `rate_limited` | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` | 1 |
| `context_saturated` | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` | 2 |
| `stuck_tui_heavy` | `heavy_capture` | `background_capture` | `throttle_capture_polling` | `defer` | 4 |

The class table is intentionally expressed in terms of existing capacity
surfaces. This keeps the workload planner a consumer of the resource cockpit,
fairness controller, context-horizon contract, herd-wave contract, and blocker
radar instead of creating a competing scheduler.

## Signal Composition

| Signal | Fresh source | Yellow/Red behavior | Stale/unavailable behavior |
| --- | --- | --- | --- |
| `context_horizon` | `docs/context-horizon-contract.md` and context-horizon DTOs | Red or black context risk defers new admission. | Fail closed to at least `defer`. |
| `blocker_radar` | `docs/blocker-radar-contract.md`, `br`, fallback Beads/git evidence | Yellow or worse claimability pressure defers. | Fail closed to at least `defer`. |
| `herd_wave` | `docs/herd-wave-contract.md` and dry-run stagger plans | Yellow recommends stagger; red or black defers burst fanout. | Fail closed to at least `defer`. |
| `resource_pressure` | `docs/resource-pressure-cockpit-contract.md` and admission summaries | Red reduces admission; black sheds idle optional work and defers non-idle work. | Fail closed to at least `defer`. |

Evidence states are ordered from strongest to weakest: `measured`, `inferred`,
`simulated`, `stale`, `unavailable`. A weaker state may only preserve or
increase conservatism. It must never upgrade a request from `defer`,
`throttle_capture_polling`, `require_human_approval`, or `shed` to `admit`.

## Dry-Run Examples

| Fleet scale | Input class | Signal condition | Decision | Notes |
| --- | --- | --- | --- | --- |
| 50 panes | `coding` | All signals measured green | `admit` | Full units admitted for ordinary coding work. |
| 100 panes | `reviewing` | Herd-wave measured yellow | `admit` | Admission remains open but recommends a 500 ms stagger. |
| 200 panes | `building` | Resource pressure measured red | `defer` | Build-heavy work waits instead of consuming local/remote proof capacity. |
| 500 panes | `context_saturated` | Context evidence stale and herd-wave red | `defer` | Context and burst evidence fail closed with a 3250 ms stagger hint. |

The examples are fixture-backed and serialized through JSON and TOON in the
Rust conformance test. They do not store raw pane content, command text,
prompts, environment values, or secrets.

## Privacy and Side Effects

`SwarmCapacityWorkloadAdmissionPlan` carries bounded workload labels, aggregate
health tiers, stable reason codes, and opaque stable IDs. It sets
`raw_pane_content_stored=false` and `side_effects_executed=false` at both plan
and decision level. Any future live surface using this DTO must keep it dry-run
until an operator-facing capacity mutation bead explicitly wires execution.
