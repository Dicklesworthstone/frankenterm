# Swarm Capacity Resource Budget Model

This document pins the `ft-b94bx.3` core-aware CPU and memory budget contract.
The model is a dry-run planning DTO. It does not spawn work, resize caches, or
execute mitigation actions.

Rust contract:
`crates/frankenterm-core/src/runtime_telemetry.rs::SwarmCapacityResourceBudgetPlan`

Rust proof:
`crates/frankenterm-core/tests/swarm_capacity_resource_budget_model.rs`

E2E smoke:
`tests/e2e/test_swarm_capacity_resource_budget_model.sh`

## Contract

The root contract id is `ft.swarm_capacity_resource_budget.v1`.

| Field | Meaning |
| --- | --- |
| `schema_version` | Versioned DTO schema, currently `1`. |
| `contract_id` | Stable contract id. |
| `generated_at_ms` | Snapshot time in epoch milliseconds. |
| `source` | Producer path or artifact source. |
| `dry_run` | Always `true`; the planner is side-effect-free. |
| `side_effects_executed` | Always `false`. |
| `hardware` | Hardware fingerprint with CPU, memory, class, class floors, evidence state, and lower-bound flag. |
| `workload_mix` | Normalized per-agent workload mix in stable class order. |
| `subsystem_budgets` | Per-subsystem budget rows. |
| `pressure_tier` | Worst per-subsystem pressure tier. |
| `lower_bound` | True when missing CPU or memory telemetry forced low defaults. |
| `reason_codes` | Bounded stable reason codes. |

## Hardware Classes

The planner uses conservative defaults:

| Class | CPU floor | Memory floor | Build slots | Child processes | Memory tier budget | SQLite/cache | Mux/render | RCH slots |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `low` | 4 | 16 GiB | 1 | 8 | 8 GiB | 256 MiB | 512 MiB | 1 |
| `mid` | 8 | 32 GiB | 3 | 32 | 20 GiB | 2 GiB | 4 GiB | 4 |
| `high` | 32 | 128 GiB | 6 | 64 | 72 GiB | 4 GiB | 8 GiB | 8 |
| `high_core` | 64 | 256 GiB | 12 | 128 | 160 GiB | 8 GiB | 16 GiB | 16 |

`high_core` is the only class that satisfies the 64+ CPU / 256 GiB target
predicate. If either CPU or memory telemetry is missing or zero, the planner
sets `hardware.lower_bound = true`, `hardware.evidence_state = unavailable`,
and uses the `low` budget class.

## Subsystem Rows

Every plan contains rows for:

- `build_slots`
- `child_processes`
- `memory_tiers`
- `sqlite_cache`
- `mux_render`
- `rch_offload`

Rows carry a unit (`slots`, `processes`, or `bytes`), budget, modeled use,
saturating available budget, saturation per 1000, pressure tier, and stable
reason codes. Arithmetic uses saturating counters so large synthetic swarms
cannot wrap budget totals.

## Target-Class Regression

The model must not turn a skipped target-class artifact into a high-scale claim.
The retained artifact
`tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json`
currently records Darwin arm64, 14 logical CPUs, and 64 GiB memory. The Rust
regression test verifies that artifact maps below `high_core` and keeps the
64+ CPU / 256 GiB claim unproven.
