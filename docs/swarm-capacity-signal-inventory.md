# Swarm Capacity Signal Inventory

Status: `ft-b94bx.1` source-of-truth inventory for the high-core swarm
capacity autopilot epic.

This document catalogs the existing signals that a future capacity planner can
consume without inventing a parallel cockpit. It intentionally does not make a
500-pane safety claim. Any such claim must flow through retained RCH or
target-class artifacts and the release attestation graph.

Machine-readable fixture: `crates/frankenterm-core/tests/fixtures/swarm_capacity_signal_inventory/complete.json`

Schema: `docs/json-schema/ft-swarm-capacity-signal-inventory.json`

## Evidence States

Rows use the union of current operator contracts:

| State | Meaning for this inventory |
| --- | --- |
| `measured` | Backed by a live DTO, retained artifact, or direct collector output. |
| `inferred` | Derived from other measured surfaces, such as Beads plus git status. |
| `simulated` | Available only through synthetic or fixture-backed proof. |
| `stale` | A known source exists, but freshness cannot currently be proven. |
| `unavailable` | A required source is missing or intentionally not collected. |
| `mixed` | The composed signal has both available and unavailable sub-signals. |

All rows are privacy-bounded. The planner may consume counters, aggregate
states, bounded identifiers, and redacted artifact paths. It must not store raw
pane content.

## Signal Inventory

| Signal ID | Capacity question | Current source path | Evidence | Privacy posture | Telemetry-gap behavior | Consumers |
| --- | --- | --- | --- | --- | --- | --- |
| `host.logical_cpu_count` | How many logical cores can admission divide across? | `docs/perf/swarm-capacity-baseline.md` | `simulated` until the target-class artifact records a hardware fingerprint. | `safe_counter` | `lower_bound_only` | `ft-b94bx.3`, `ft-b94bx.5` |
| `host.cpu_saturation` | Is the operator host CPU saturated before adding panes or builds? | `docs/perf/swarm-capacity-baseline.md` | `unavailable`; no committed live CPU saturation DTO is tied to capacity admission yet. | `safe_counter` | `emit_unavailable` and refuse target-class claims. | `ft-b94bx.3`, `ft-b94bx.7` |
| `host.memory_total_bytes` | Is the host in a high-memory target class such as 256 GiB? | `docs/perf/swarm-capacity-baseline.md` | `simulated` unless supplied by retained target-class proof. | `safe_counter` | `lower_bound_only` | `ft-b94bx.3`, `ft-b94bx.5` |
| `process.rss_residency_buckets` | Which residency buckets are pressuring the process? | `crates/frankenterm-core/src/memory_pressure.rs` | `measured` through `MacosResidencyBucket` when the platform collector is available; otherwise `unavailable`. | `aggregate_only` | `emit_unavailable` for missing buckets instead of omitting them. | `ft-b94bx.3`, `ft-b94bx.7` |
| `fleet.memory_tiers` | Which fleet memory tier is the limiting resource? | `crates/frankenterm-core/src/fleet_memory_controller.rs` | `measured` through fleet-memory tier summaries. | `aggregate_only` | `fail_closed` when tier summaries are stale or missing. | `ft-b94bx.3`, `ft-b94bx.6` |
| `capture.scheduler_budget` | Is capture work respecting global and per-pane budgets? | `crates/frankenterm-core/src/tailer.rs` plus `docs/capture-fairness-slo-contract.md` | `measured` when `CaptureScheduler` snapshots are present. | `bounded_identifier` | `emit_stale` when freshness exceeds the SLO window. | `ft-b94bx.3`, `ft-b94bx.6` |
| `capture.skipped_poll_reasons` | Why did a pane miss capture service? | `docs/capture-fairness-slo-contract.md` | `mixed`; reason vocabulary exists, but planner-specific aggregation is not yet a DTO. | `bounded_identifier` | `emit_unavailable` per missing reason family. | `ft-b94bx.7`, `ft-b94bx.10` |
| `context.horizon_risk` | Which panes are context-saturated or near rotation thresholds? | `crates/frankenterm-core/src/context_horizon.rs` plus `docs/context-horizon-contract.md` | `measured` for horizon DTOs; `inferred` for workload-class pressure. | `no_raw_content` | `fail_closed`; recommendations remain dry-run when evidence is stale. | `ft-b94bx.2`, `ft-b94bx.6` |
| `herd.wave_pressure` | Is synchronized swarm activity creating burst pressure? | `crates/frankenterm-core/src/swarm_scheduler.rs` plus `docs/herd-wave-contract.md` | `measured` for herd-wave summaries and dry-run stagger plans. | `bounded_identifier` | `fail_closed`; do not mutate admission when evidence is stale. | `ft-b94bx.2`, `ft-b94bx.6` |
| `blocker.claimability` | Are apparent ready beads actually claimable and non-overlapping? | `crates/frankenterm-core/src/blocker_radar.rs`, `scripts/swarm-tick.sh`, `docs/blocker-radar-contract.md` | `inferred` from `br`, advisory `bv`, git status, RCH, CI, and Agent Mail health. | `bounded_identifier` | `emit_stale` for stale owners and `emit_unavailable` for mail outages. | `ft-b94bx.2`, `ft-b94bx.9` |
| `storage.io_pressure` | Is storage I/O the limiting resource for capture or search? | `crates/frankenterm-core/src/storage/io_scheduler.rs` | `measured` through `StorageIoOperatorSummary`. | `aggregate_only` | `fail_closed` when write errors or queue saturation appear. | `ft-b94bx.3`, `ft-b94bx.7` |
| `sqlite.persistence_pressure` | Are SQLite/page-cache/search-lag paths reducing safe fanout? | `docs/resource-pressure-cockpit-contract.md` | `mixed`; storage I/O is measured, but page-cache attribution is not a capacity row yet. | `aggregate_only` | `emit_unavailable` for missing page-cache detail. | `ft-b94bx.3`, `ft-b94bx.7` |
| `build.rch_worker_pool` | Can heavy proof/build work be offloaded without local fallback? | `tests/e2e/lib_rch_guards.sh` | `measured` in RCH proof metadata when `run_rch_cargo_logged` reaches a remote worker. | `redacted_artifact_path` | `fail_closed`; remote-required proof refuses local Cargo fallback. | `ft-b94bx.5`, `ft-b94bx.10` |
| `build.local_slot_pressure` | Are local build slots or compiler jobs competing with pane capacity? | `scripts/validate_asupersync_rch_execution_policy.sh` | `unavailable`; current policy forbids local heavy Cargo for this lane. | `safe_counter` | `emit_unavailable` and keep build pressure out of target-class claims. | `ft-b94bx.3`, `ft-b94bx.7` |
| `mux.render_pressure` | Is mux/render work saturating before the capture or storage layers do? | `docs/resource-pressure-cockpit-contract.md` | `unavailable`; no planner-owned render pressure row is committed yet. | `aggregate_only` | `emit_unavailable`; downstream planner must degrade confidence. | `ft-b94bx.3`, `ft-b94bx.7` |
| `child_process.pressure` | Are child processes consuming RSS/CPU outside the parent process budget? | `docs/resource-pressure-cockpit-contract.md` | `mixed`; child-process residency bucket exists, but CPU and per-agent attribution are gaps. | `aggregate_only` | `emit_unavailable` for attribution and `lower_bound_only` for RSS. | `ft-b94bx.3`, `ft-b94bx.7` |
| `attestation.capacity_artifacts` | Are high-scale claims tied to retained artifacts? | `docs/attestations/manifest.json` and `docs/attestations/proofs/resource-cockpit-target-class.json` | `measured` for existing resource cockpit target-class proof slots. | `redacted_artifact_path` | `defer_target_class_claim` when the manifest lacks the required category. | `ft-b94bx.5`, `ft-b94bx.8`, `ft-b94bx.10` |
| `workload.classification` | Which panes are coding, reviewing, building, blocked, idle, rate-limited, context-saturated, or TUI-heavy? | `crates/frankenterm-core/src/runtime_telemetry.rs` | `simulated`; `SwarmCapacityWorkClass` exists, but per-agent classifier output is future work. | `bounded_identifier` | `lower_bound_only` and avoid upgrading admission from missing class data. | `ft-b94bx.2`, `ft-b94bx.4` |

## Gap Map

| Gap ID | Required for | Current state | Missing sources | Fail-closed behavior |
| --- | --- | --- | --- | --- |
| `cpu_core_saturation` | Core-aware budget rows and 64+ CPU claims. | No live capacity DTO records CPU saturation by core class. | Host load, per-core utilization, and pane/build attribution. | Planner reports `unavailable` and refuses target-class safety claims. |
| `per_agent_workload_class` | Workload-class admission and stagger decisions. | Work-class enums exist, but no per-agent classifier artifact is retained. | Per-pane class snapshot and classifier freshness. | Planner uses lower-bound assumptions and never upgrades stale evidence. |
| `build_pressure` | Capacity planning while tests/builds run. | RCH worker proof exists; local build pressure is intentionally unavailable. | Queue depth, active compiler jobs, and remote/local split summary. | Planner refuses local-heavy proof and emits `unavailable` for local slots. |
| `mux_render_pressure` | GUI/mux saturation detection before capture falls behind. | No planner-owned render pressure row. | Frame/render latency, mux event-loop lag, and GPU/media pressure. | Planner lowers confidence and does not treat capture health as render health. |
| `disk_sqlite_pressure` | Storage-aware pane fanout and persistence safety. | Storage I/O pressure exists; page-cache and SQLite write-lag attribution is incomplete. | SQLite page-cache pressure, write-ahead log growth, and search index lag. | Planner emits `mixed` or `unavailable` and fails closed on write errors. |
| `child_process_pressure` | Correct host-level budget for agent subprocesses. | RSS bucket can expose child-process bytes; CPU and ownership attribution are gaps. | Child-process CPU, per-agent descendants, and stale-process lifetime. | Planner uses lower-bound RSS only and refuses exact headroom claims. |

## Non-overlap With `ft-tf6g3.46`

`ft-b94bx.1` owns the capacity signal inventory, source-path map, privacy
posture, and telemetry-gap behavior. It does not re-audit the full
cross-family invariant matrix.

`ft-tf6g3.46` remains the owner for cross-family invariant pass/fail evidence.
Capacity planner rows may cite those invariants as consumers or prerequisites,
but must not duplicate their acceptance criteria. When an invariant is missing,
this inventory records the planner-visible gap and the fail-closed behavior.
