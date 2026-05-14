# Swarm Capacity Workload Admission Model

Status: `ft-b94bx.2` contract and dry-run model for the high-core swarm
capacity autopilot epic.

This document defines the first workload-class layer above the existing
capacity controller. It consumes the signal inventory from
`docs/swarm-capacity-signal-inventory.md` and maps per-agent workload classes to
the already-shipped `SwarmCapacityAdmissionRequest` vocabulary. It does not
execute pane, queue, mux, or workflow mutations.

Machine-readable Rust DTO:
`crates/frankenterm-core/src/runtime_telemetry.rs` under
`SwarmCapacityWorkloadAdmissionPlan`.

Rust conformance test:
`crates/frankenterm-core/tests/swarm_capacity_workload_admission_model.rs`.

E2E smoke:
`tests/e2e/test_swarm_capacity_workload_admission_model.sh`.

## Contract Envelope

The root contract id is `ft.swarm_capacity_workload_admission.v1`.

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version. Version 1 is this contract. |
| `contract_id` | Stable contract id. |
| `generated_at_ms` | Snapshot timestamp. |
| `source` | Producer path, for example `swarm_capacity.workload_admission.examples`. |
| `dry_run` | Always `true` for this model. |
| `raw_pane_content_stored` | Always `false`. |
| `side_effects_executed` | Always `false`. |
| `admission_table` | Deterministic class table for every workload class. |
| `decisions` | One dry-run decision per request. |
| `reason_codes` | Bounded stable reasons collected from decisions. |

The DTO may be serialized as JSON or TOON. JSON/TOON parity must preserve the
same decoded fields; the test suite rejects format-specific drift.

## Workload Classes

| Workload class | Meaning | Existing workload surface | Existing pressure class | Green action | Missing/stale evidence floor |
| --- | --- | --- | --- | --- | --- |
| `coding` | Active implementation work. | `backpressure_escalation` | `claimed_agent_task` | `admit` | `defer` |
| `reviewing` | Review, audit, or read-heavy checking. | `backpressure_escalation` | `claimed_agent_task` | `admit` | `defer` |
| `building` | Build or compile-heavy work. | `capacity_governor` | `claimed_agent_task` | `admit` | `defer` |
| `testing` | Test, fuzz, or verifier execution. | `capacity_governor` | `claimed_agent_task` | `admit` | `defer` |
| `idle` | Idle pane or wait-only loop. | `idle_observation` | `optional_diagnostics` | `admit` | `defer` |
| `blocked` | Pane blocked on ownership, infra, or dependency. | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` |
| `rate_limited` | Pane is rate-limited and should not receive more fanout. | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` |
| `context_saturated` | Pane is near or past the context horizon. | `backpressure_escalation` | `claimed_agent_task` | `defer` | `defer` |
| `stuck_tui_heavy` | TUI-heavy or stuck pane raising render/capture pressure. | `heavy_capture` | `background_capture` | `throttle_capture_polling` | `defer` |

## Composed Signals

Every decision composes four signal families. The planner treats missing or
stale evidence as a lower-bound capacity claim and never upgrades admission from
it.

| Signal | Source family | Green use | Degraded use |
| --- | --- | --- | --- |
| `context_horizon` | `crates/frankenterm-core/src/context_horizon.rs` and `docs/context-horizon-contract.md` | Context pressure can remain informational while green/yellow. | Red or black context pressure defers new admission. Stale or unavailable evidence defers. |
| `blocker_radar` | `crates/frankenterm-core/src/blocker_radar.rs` and `docs/blocker-radar-contract.md` | Actionable claimability allows normal class behavior. | Yellow/red/black claimability defers; stale or unavailable evidence defers. |
| `herd_wave` | `crates/frankenterm-core/src/swarm_scheduler.rs` and `docs/herd-wave-contract.md` | Yellow pressure may keep the class action but adds `recommended_stagger_ms`. | Red/black burst pressure defers fanout; stale or unavailable evidence defers. |
| `resource_pressure` | Resource pressure cockpit and capacity controller summaries. | Green/yellow pressure can use the class baseline. | Red pressure reduces admission; TUI-heavy rows throttle capture polling. Black pressure sheds idle rows and defers others. |

## Decision Table

The model composes each class baseline with the most conservative signal floor.
Action rank is:

| Rank | Action |
| --- | --- |
| 0 | `admit` |
| 1 | `throttle_capture_polling` |
| 2 | `defer` |
| 3 | `require_human_approval` |
| 4 | `shed` |

For every class and signal family, degrading evidence from `measured` to
`stale` or `unavailable` must not reduce the action rank. The conformance test
iterates the full class x signal x pressure grid to enforce that invariant.

## Dry-Run Examples

The helper `swarm_capacity_workload_admission_dry_run_examples()` emits four
deterministic examples:

| Pane scale | Workload class | Signal posture | Expected action | Notes |
| --- | --- | --- | --- | --- |
| 50 | `coding` | All measured green. | `admit` | Baseline implementation work. |
| 100 | `reviewing` | Herd-wave yellow. | `admit` with stagger | Uses `recommended_stagger_ms` without claiming pressure is safe for mutation. |
| 200 | `building` | Resource pressure red. | `defer` | Build pressure slows admission instead of adding local compile load. |
| 500 | `context_saturated` | Context stale/red and herd-wave red. | `defer` | Missing freshness fails closed and avoids fanout. |

These examples are synthetic contract rows. They are not target-class 64+ CPU or
256 GiB proof and must not be cited as production safety evidence.

## Privacy

The workload-class model is identifier- and counter-only. It may carry stable
opaque ids, workload class names, pressure tiers, reason codes, and redacted
artifact paths. It must never store or emit raw pane transcripts, prompt bodies,
command text, cwd, cookies, bearer tokens, API keys, or session secrets.
