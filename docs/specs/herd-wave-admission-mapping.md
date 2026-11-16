# Herd-Wave Admission Spec Mapping

Spec: `herd-wave-admission.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `active` / `Panes` | `crates/frankenterm-core/src/swarm_scheduler.rs:137` | Abstracts pane-scoped `HerdWaveSignal` rows into a distinct cohort. |
| `PressureFor` | `crates/frankenterm-core/src/swarm_scheduler.rs:2508` | Mirrors `herd_wave_pressure_tier` threshold ordering. |
| `DelayForRank` | `crates/frankenterm-core/src/swarm_scheduler.rs:738` | Mirrors bounded per-rank stagger delay. |
| `calendar` | `crates/frankenterm-core/src/swarm_scheduler.rs:220` and `:262` | Abstracts `HerdWaveStaggeredAction` and `HerdWaveDryRunCalendarEntry`. |
| `source_state` | `crates/frankenterm-core/src/swarm_scheduler.rs:446` | Abstracts freshness/evidence rows into fresh, missing, and stale states. |
| `priority_units` | `crates/frankenterm-core/src/swarm_scheduler.rs:2219` | Abstracts pane priority, mission criticality, work priority, and override cap. |
| `cooldown_active` / `circuit_active` | `docs/herd-wave-contract.md:97` | Contract states represented through dry-run notes and capacity-controller posture. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Evaluate` | `crates/frankenterm-core/src/swarm_scheduler.rs:746` | `detect_herd_wave_pressure` computes the current cohort pressure summary. |
| `Evaluate` | `crates/frankenterm-core/src/swarm_scheduler.rs:1974` | `SwarmAdmissionController::evaluate` combines pressure, missing telemetry, and priority protection into an admission decision. |
| `Evaluate` | `crates/frankenterm-core/src/swarm_scheduler.rs:2336` | `plan_herd_wave_dry_run_actions` turns the detected cohort and admission summary into dry-run calendar rows. |
| `CalendarFor` | `crates/frankenterm-core/src/swarm_scheduler.rs:2258` | `plan_herd_wave_staggered_actions` orders distinct panes by earliest signal time and pane id; the model uses numeric pane order as the deterministic tie-free abstraction. |
| `OverallStateFor` | `crates/frankenterm-core/src/swarm_scheduler.rs:2650` | `root_overall_state` projects missing/stale telemetry, priority protection, override, and pressure tier to operator state. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `PressureMatchesThresholds` | `crates/frankenterm-core/src/swarm_scheduler.rs:788` | Distinct pane count determines detected pressure tier. |
| `AdmissionMatchesEffectiveSeverity` | `crates/frankenterm-core/src/swarm_scheduler.rs:2072` and `:2241` | Priority protection lowers severity before `action_for_severity` maps to the admission action. |
| `MissingTelemetryFailsClosed` | `crates/frankenterm-core/src/swarm_scheduler.rs:2081` | Missing telemetry converts an otherwise-admitted decision to `defer` with a fail-closed reason. |
| `StaleEvidenceNotNormal` | `crates/frankenterm-core/src/swarm_scheduler.rs:2629` and `:2650` | Stale source freshness projects to stale evidence rather than a normal trusted state. |
| `PriorityProtectionOnlyReducesSeverity` | `crates/frankenterm-core/src/swarm_scheduler.rs:591` | Priority protection is exposed explicitly and cannot hide the raw pressure. |
| `StaggerDelayIsBoundedAndRanked` | `crates/frankenterm-core/src/swarm_scheduler.rs:736` | Each cohort rank maps to a bounded delay capped by `max_stagger_ms`. |
| `DryRunNeverMutates` | `crates/frankenterm-core/src/swarm_scheduler.rs:289` | `HerdWaveDryRunPlan` carries `dry_run_only=true` and `live_mutation_allowed=false`. |
| `CooldownAndCircuitStayReadOnly` | `docs/herd-wave-contract.md:239` | Operator runbook treats cooldown and circuit-breaker states as manual-review/read-only posture. |
| `HistoryRowsMatchPlanner` | `crates/frankenterm-core/src/swarm_scheduler.rs:2675` | Reason/state projection is derived from the same summary and source rows represented in the root snapshot. |

## TLC Configuration

Config: `herd-wave-admission.cfg`

The deterministic smoke model uses four panes with compressed thresholds
(`MinDistinct = 2`, `CriticalDistinct = 3`, `EmergencyDistinct = 4`) so one TLC
run covers normal, elevated, critical, and emergency tiers. `BaseStaggerMs = 5`
and `MaxStaggerMs = 10` cover rank-0/no-delay, regular stagger, and capped tail
delay behavior. The release-bundle proof slot is
`proofs/herd-wave-admission.json`.
