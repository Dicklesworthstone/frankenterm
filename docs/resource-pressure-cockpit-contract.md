# Resource Pressure Cockpit Contract

Status: v1 contract with retained remote-reduced conformance proof; target-hardware
proof still required for high-scale claims

This document defines the versioned operator-facing contract for the resource
pressure cockpit. The cockpit is the single shape that future `ft doctor`,
`ft robot capacity`, Robot, and MCP surfaces should emit when answering:

- what resource domain is pressured,
- whether the evidence is measured, simulated, unavailable, or stale,
- what work was admitted, deferred, degraded, or shed,
- what mitigation or mutation was actually applied.

The JSON schema sketch lives at
`docs/json-schema/ft-resource-pressure-cockpit.json`.

Current retained v1 conformance evidence lives at
`tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json`.
That run passed local static checks and the remote-reduced schema/runtime lane,
with remote Cargo, rustc, and test-binary execution observed on an RCH worker. It
is the reference artifact for this contract's schema and runtime conformance
wording. It is not target-hardware proof: the same summary records
`target_hardware = "skipped_not_proven"`.

## Existing Anchors

The contract intentionally matches names that already exist in code and
operator docs instead of creating a parallel vocabulary.

| Surface | Existing field or type | Contract use |
| --- | --- | --- |
| `README.md` fleet memory controller | Queue depth, system memory, per-pane budgets, four-tier pressure | Defines the top-level pressure domains and normal/elevated/critical/emergency mapping. |
| `docs/high-core-swarm-runbook.md` | `.swarm_capacity.resource_cockpit` | Keeps the cockpit under the existing doctor/robot capacity output path. |
| `SwarmResourceCockpitSnapshot` | `schema_version`, `status`, `proof_gate`, `memory_pressure`, `memory_tiers`, `resource_admission_decisions`, `storage_io`, `mitigation_history`, `drilldowns` | Baseline live snapshot that this contract extends. |
| `FleetPressureTier` | `normal`, `elevated`, `critical`, `emergency` | Shared pressure language for fleet memory, pane budget, worker-pool, and resource admission domains. |
| `MemoryPressureTier` | `green`, `yellow`, `orange`, `red` | Host memory pressure input before it is synthesized into fleet pressure. |
| `FleetMemoryTier` | `hot_resident`, `warm_compressed`, `cold_disk`, `search_index_cache`, `render_cache`, `transient_ingestion`, `allocator_pools` | Required memory-tier rows. |
| `BackpressureTier` policy docs | `green`, `yellow`, `red`, `black` | Queue and pipeline backpressure rows. |
| `StorageIoOperatorSummary` | `io_pressure_tier`, `io_pressure_reason`, queue, lag, audit, and write-error counters | Storage IO remains a separate domain, not a subfield of memory or CPU. |
| `ResourceAdmissionDecisionSummary` | `admit`, `defer`, `degrade`, `shed` plus reason codes and pressure inputs | Resource admission rows. |
| Blessed tuning and safe auto-tuning docs | `evidence_level`, `target_hardware`, `skipped_not_proven` | Proof gating for high-scale and live mutation claims. |

## Output Surfaces

The canonical location remains:

```text
.swarm_capacity.resource_cockpit
```

Required surfaces for later implementation:

| Command or API | Required posture |
| --- | --- |
| `ft doctor --json` | Emits the full cockpit when capacity transparency is available; emits unavailable domains instead of omitting them. |
| `ft robot capacity --level 2` | Emits the cockpit plus compact rows suitable for agent orchestration. |
| `ft robot --format toon capacity --level 2` | Preserves all reason codes and evidence states while using compact field names only where already documented. |
| MCP/Robot resource endpoints | Reuse the same schema version and reason codes; no separate MCP-only resource vocabulary. |

## Versioned Envelope

The contract id is `ft.resource_pressure_cockpit.v1`. The root object must
carry:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version. Version 1 matches `SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION`. |
| `contract_id` | Stable string, currently `ft.resource_pressure_cockpit.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for this snapshot. |
| `source` | Producer path, for example `runtime_telemetry.swarm_resource_cockpit`. |
| `status` | Existing `SwarmCapacityOperatorStatus`: `ready`, `watch`, `violated`, `unknown`, or `unavailable`. |
| `proof_gate` | Existing cockpit proof gate: `healthy`, `pressured`, `degraded`, or `skipped_proof`. |
| `evidence_state` | Top-level synthesis of domain states: `measured`, `simulated`, `unavailable`, `stale`, or `mixed`. |
| `summary` | One-line operator summary. |
| `next_operator_move` | Concrete next step. Empty or vague text is not acceptable when pressure is non-green. |
| `run_identity` | Run id, evidence level, repo/artifact pointers, and hardware predicate. |
| `domains` | Required domain summaries, including unavailable domains. |
| `memory_tiers` | Bounded rows for hot/warm/cold/cache/buffer/allocator memory tiers. |
| `residency_buckets` | RSS/residency attribution rows. |
| `queue_backpressure` | Capture, write, event-bus, persistence, search, cold-tier, and admission queue rows. |
| `storage_io` | Optional detailed storage IO summary using the existing storage scheduler fields. |
| `worker_pool` | Optional worker-pool pressure summary. |
| `admission_decisions` | Capacity and resource admission decisions. |
| `action_receipts` | Planned, dry-run, applied, blocked, failed, compensated, or rollback receipts. |
| `mitigation_history` | Recent mitigations distilled for the operator view. |
| `drilldowns` | Stable subject/reason/detail rows explaining every non-green or unknown domain. |
| `artifact_paths` | Retained artifacts needed to reproduce or audit the snapshot. |

The live Rust snapshot may initially omit fields that are not yet wired.
Implementation beads must converge toward this full root shape without changing
the already-live field names.

## Evidence States

Missing telemetry is not neutral. Every domain and action receipt must declare
its evidence state.

| State | Meaning | Allowed operator use |
| --- | --- | --- |
| `measured` | Collected from the live host/process/run represented by this snapshot. | May support operational decisions if fresh and proof-gated. |
| `simulated` | Generated by replay, fixture, dry-run, synthetic load, or model-only evaluation. | May support development and planning; cannot prove high-scale production claims. |
| `unavailable` | The producer could not collect the telemetry or the subsystem is not wired. | Must force `skipped_proof` or `degraded` at the relevant gate; never treated as green. |
| `stale` | Telemetry exists but is older than the domain freshness budget or from another run. | Cannot support live mutation, auto-tuning, or high-scale promotion. |
| `mixed` | The root object combines domains with different states. | Root must list each per-domain state and reason code. |

Freshness fields are required for every domain:

| Field | Meaning |
| --- | --- |
| `generated_at_ms` | When that domain's source sample was generated; nullable only when unavailable. |
| `freshness_ms` | Age of the source sample at root generation time. |
| `max_age_ms` | Domain-specific freshness budget. |
| `source` | Stable producer name. |
| `reason_codes` | Stable reasons explaining unavailable, stale, simulated, or non-green states. |

## Domains

The `domains` object must include these keys even when evidence is unavailable:

| Domain | Required pressure vocabulary | Notes |
| --- | --- | --- |
| `memory` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Synthesizes host memory, tier budgets, and resource admission pressure. |
| `rss_residency` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Separates heap growth from mmap/file-backed, graphics/media, SQLite cache, child process, and unknown residency. |
| `pane_budget` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Summarizes per-pane memory budgets and refused bytes. |
| `queue_backpressure` | `green`, `yellow`, `red`, `black`, or `unknown` | Covers capture, write, persistence, event bus, search, cold-tier hydration, and admission queues. |
| `storage_io` | `green`, `yellow`, `red`, `black`, or `unknown` | Uses `StorageIoOperatorSummary`; storage pressure is independent from CPU/memory. |
| `worker_pool` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Summarizes healthy/busy/waiting workers and queue saturation. |
| `capacity_admission` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Capacity-controller decisions. |
| `resource_admission` | `normal`, `elevated`, `critical`, `emergency`, or `unknown` | Global resource admission decisions. |
| `action_receipts` | `green`, `yellow`, `red`, `black`, or `unknown` | Receipt health for mitigations, dry-runs, mutations, rollbacks, and compensation. |

Every domain row must contain `summary`, `operator_action`, and `reason_codes`.
If the domain is green/normal but evidence is not measured and fresh, the row is
not green; use `unknown` plus a telemetry reason code.

## Memory And Residency

`memory_tiers` must use the existing `FleetMemoryTier` names:

- `hot_resident`
- `warm_compressed`
- `cold_disk`
- `search_index_cache`
- `render_cache`
- `transient_ingestion`
- `allocator_pools`

Each row should carry budget, actual, over-budget, remaining, reclaimable,
reclaimed, evicted, and refused bytes where available. `cold_disk` is budgeted
but does not directly reclaim RSS.

`residency_buckets` are required for live memory incident work:

| Bucket | Purpose |
| --- | --- |
| `rust_heap` | Allocator-owned heap and long-lived Rust structures. |
| `mmap_file_backed` | File-backed mmap, cold scrollback, Tantivy, or SQLite mappings. |
| `sqlite_page_cache` | SQLite cache and WAL/page-cache residency where distinguishable. |
| `graphics_media` | GPU, image, font, and render/media residency. |
| `scrollback_cache` | Hot and warm scrollback buffers not already accounted elsewhere. |
| `child_processes` | Child process RSS attributable to the same fleet run. |
| `unknown` | Unattributed resident bytes. Non-zero unknown pressure must create a drilldown. |

The cockpit must not collapse heap growth and file-backed residency into a
single "memory leak" claim. Incident tooling can still summarize, but the
machine contract keeps the attribution rows separate.

## Queue And Backpressure

`queue_backpressure` rows use the documented backpressure tiers:
`green`, `yellow`, `red`, `black`, or `unknown`.

Required queue names:

- `capture`
- `write`
- `persistence`
- `event_bus`
- `search_indexing`
- `cold_tier_hydration`
- `resource_admission`

Rows should include queue depth, capacity, utilization, oldest queued age, and
operator action when available. Existing gap semantics such as
`backpressure_pause`, `backpressure_overflow`, and `backpressure_resume` remain
event/gap reasons; cockpit rows should translate them into stable
`resource.queue.*` reason codes.

## Storage IO

The detailed `storage_io` object follows `StorageIoOperatorSummary`:

- `schema_version`
- `pressure_domain`
- `io_pressure_tier`
- `io_pressure_reason`
- `operator_action`
- `aggregate_queue_depth`
- `aggregate_bytes_pending`
- `oldest_queued_age_ms`
- `durability_pending_total`
- `search_lag_segments`
- `hydration_lag_pages`
- `audit_fail_closed_total`
- `write_error_total`
- `dominant_class`

Storage IO reason codes may use the storage scheduler reasons directly under a
domain prefix, for example `storage_io.defer.queue_full` or
`storage_io.fail_closed.io_error`.

## Worker Pool Pressure

The worker-pool domain is for live swarm capacity and remote execution
pressure, not for proving that a specific RCH command selected the intended
worker. The row should include:

- total, healthy, busy, waiting, and degraded worker counts,
- queue depth and oldest queued age,
- saturation pressure,
- whether local fallback is allowed for this operation,
- reason codes for no healthy workers, saturation, stale inventory, or forbidden
  local fallback.

RCH proof-ledger worker mirror evidence remains in the RCH policy documents.
The resource cockpit may point to those artifacts but must not replace them.

## Admission Decisions

`admission_decisions` must include both capacity and global resource admission
rows when present.

Resource admission actions must use the existing enum:

- `admit`
- `defer`
- `degrade`
- `shed`

Resource reason codes must use the existing stable names when they originate in
`AdmissionReasonCode`:

- `healthy`
- `queue_elevated`
- `queue_saturated`
- `queue_over_capacity`
- `failure_rate_high`
- `fleet_pressure`
- `memory_tier_pressure`
- `latency_stage_over_budget`
- `herd_wave_pressure`
- `missing_queue_telemetry`
- `missing_fleet_telemetry`
- `missing_memory_tier_telemetry`
- `missing_latency_telemetry`
- `non_finite_telemetry`
- `invalid_latency_telemetry`
- `priority_protected`
- `operator_override`
- `fail_closed_missing_telemetry`

If missing telemetry changes an otherwise `admit` decision, the decision must
carry `fail_closed_missing_telemetry` and the relevant domain must be
`unavailable` or `stale`.

## Action Receipts

Action receipts are the bridge between "the cockpit recommended a mitigation"
and "the system actually did something." They are intentionally separate from
`mitigation_history`, which is a compact operator summary.

Each receipt must include:

| Field | Meaning |
| --- | --- |
| `receipt_id` | Stable idempotency or audit id. |
| `action` | Requested mitigation or mutation. |
| `target_domain` | Domain the action is meant to relieve. |
| `requested_at_ms` | Request timestamp. |
| `completed_at_ms` | Completion timestamp when known. |
| `status` | `planned`, `dry_run`, `applied`, `succeeded`, `blocked`, `failed`, `compensated`, `compensation_failed`, `rollback_required`, or equivalent fleet mutation status. |
| `dry_run` | True when no side effect was attempted. |
| `policy_decision` | `allow`, `deny`, `require_approval`, `not_checked`, or a narrower policy result. |
| `evidence_state` | Evidence state for the receipt itself. |
| `reason_codes` | Stable reasons for the outcome. |
| `artifact_paths` | Proof, audit, or replay artifacts. |

A recommendation without a receipt is only a recommendation. A mutation without
a receipt is a defect in the later implementation.

### Operator Interpretation

Pressure actions are operational controls, not source-code failure verdicts.
`delay_admission`, `degrade_capture`, `shed_optional_work`, `compress_scrollback`,
and `evict_scrollback` mean the cockpit is preserving service under current
resource evidence. Operators should first inspect `evidence_state`,
`policy_decision`, and `reason_codes`:

- `dry_run` or `planned` receipts prove intent only; they do not prove a side
  effect happened.
- `blocked` receipts with `admission.fail_closed.missing_telemetry` mean the
  system refused to act because required telemetry was absent or stale enough to
  make the action unsafe.
- `applied` or `succeeded` receipts require a correlation id, affected resource
  attribution when known, and artifact paths before they can be cited as proof.
- `failed`, `compensation_failed`, or `rollback_required` receipts are
  operational incidents for that action lane. They are not evidence that the
  underlying pane, agent, or connector is defective without separate drilldown
  evidence.

## Reason Codes

Reason codes are low-cardinality, machine-readable strings. Use existing code
enum values directly where available; use the dotted prefixes below for cockpit
level reasons.

| Prefix | Examples |
| --- | --- |
| `resource.proof.*` | `resource.proof.healthy`, `resource.proof.skipped`, `resource.proof.target_hardware_missing` |
| `resource.telemetry.*` | `resource.telemetry.unavailable`, `resource.telemetry.stale`, `resource.telemetry.simulated`, `resource.telemetry.mixed` |
| `resource.memory.*` | `resource.memory.tier_pressure`, `resource.memory.rss_growth`, `resource.memory.heap_growth`, `resource.memory.mmap_residency`, `resource.memory.unknown_residency` |
| `resource.queue.*` | `resource.queue.capture_pressure`, `resource.queue.write_pressure`, `resource.queue.event_bus_lag`, `resource.queue.persistence_lag`, `resource.queue.backpressure_overflow` |
| `storage_io.*` | `storage_io.defer.queue_full`, `storage_io.degrade.oldest_age_exceeded`, `storage_io.fail_closed.io_error`, `storage_io.defer.search_freshness_lag`, `storage_io.fail_closed.cold_tier_unavailable` |
| `worker_pool.*` | `worker_pool.no_healthy_workers`, `worker_pool.busy_wait`, `worker_pool.stale_inventory`, `worker_pool.local_fallback_forbidden` |
| `admission.*` | `admission.defer.pressure`, `admission.degrade.pressure`, `admission.shed.optional`, `admission.fail_closed.missing_telemetry` |
| `action_receipt.*` | `action_receipt.applied`, `action_receipt.blocked`, `action_receipt.dry_run`, `action_receipt.failed`, `action_receipt.rollback_required` |

Every `unknown`, `unavailable`, `stale`, `degraded`, `critical`, `emergency`,
`red`, or `black` row must have at least one reason code and at least one
drilldown.

## Proof Requirements

Static documentation and schema validation may run locally. Any later
implementation bead that compiles, tests, runs Cargo, performs live swarm
exercise, or claims runtime behavior must use RCH.

Required proof posture for future implementation:

| Claim | Minimum proof |
| --- | --- |
| Schema/doc shape only | Local static checks such as `jq empty`, markdown grep, and `git diff --check`. |
| Rust type or CLI emission compiles | `rch exec -- cargo check ...` with retained command output. |
| Robot/doctor output emits the cockpit | `rch exec -- cargo test ...` or RCH-backed e2e harness with artifact paths retained. |
| High-scale or 200+ pane cockpit claim | Target hardware predicate from `ft doctor --json`: at least 64 logical CPUs and 256 GiB memory, plus retained artifacts. Otherwise report `skipped_not_proven`. |
| Auto-tuning or live mutation | Fresh measured cockpit, proof gate not `skipped_proof`, action receipts retained, rollback/cooldown behavior tested. |

RCH logs are not proof by themselves. The retained artifact must show the command
reached Cargo/test/runtime as appropriate, and high-scale claims must show the
target hardware predicate.

The target-class hardware contract is
[`docs/perf/target-class-hardware.md`](perf/target-class-hardware.md). The gate
harness is:

```bash
FT_TARGET_CLASS_SKU=linux-x86_64-high-core scripts/run-target-class-cockpit.sh
```

It writes per-SKU summaries at
`tests/e2e/artifacts/target-class/<sku>/<run_id>/summary.json`. A
`skipped_not_proven` summary is a retained blocker artifact, not proof for
high-scale wording.

The current v1 retained conformance artifact is:

```text
tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json
```

Its status is `passed`, with `local_static = "passed"`,
`remote_reduced = "passed"`, `target_hardware = "skipped_not_proven"`,
`remote_cargo_reached = true`, `remote_rustc_reached = true`, and
`test_binary_reached = true`. Cite it only for schema/runtime conformance and
remote-reduced proof. A 64-core / 256 GiB or 200+ pane resource claim still
requires a separate target-class hardware artifact.

The current retained target-class gate artifact is:

```text
tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json
```

It is intentionally `skipped_not_proven` because the observed host and current
RCH worker capability set do not satisfy the 64 logical CPU / 256 GiB
predicate.

### RCH Soak Harness

The first `ft-p3457.4` proof lane is intentionally reduced and fail-closed:

```bash
RCH_STEP_TIMEOUT_SECS=1800 tests/e2e/test_ft_p3457_4_resource_pressure_soak.sh
```

The harness refuses local heavy Cargo by using the shared RCH guard library with
`RCH_REQUIRE_REMOTE=1`. It retains `commands.txt`, `env.txt`, `structured.log`,
`host_capability.json`, `proof-ledger.jsonl`, before/during/after cockpit
snapshot artifacts, and the remote `cargo test -p frankenterm-core
resource_pressure_ --lib -- --nocapture` output. The summary explicitly
separates measured remote execution and host capability from simulated
scaled-equivalent pressure snapshots. A 200-pane or high-scale cockpit claim
remains `skipped_not_proven` unless the remote host predicate is measured at
64+ logical CPUs and 256+ GiB memory and a later live soak captures the runtime
cockpit artifacts.

### Memory Incident Artifact Checklist

When an operator reports high memory or a possible leak, collect artifacts that
preserve the cockpit's residency split instead of flattening everything into a
single memory number:

```bash
ft triage -f json > /tmp/ft-memory-triage.json
ft status --health > /tmp/ft-memory-status-health.txt
ft doctor --json > /tmp/ft-memory-doctor.json
ft robot events --limit 100 > /tmp/ft-memory-events.json
ft diag bundle --output /tmp/ft-memory-diag
ft robot capacity --level 2 > /tmp/ft-memory-cockpit.json
ps -axo pid,ppid,rss,vsz,comm | rg 'frankenterm|ft |wezterm'
```

On macOS, add native process evidence for the suspected FrankenTerm or mux PID:

```bash
vmmap <pid> -summary > /tmp/frankenterm-<pid>.vmmap.txt
/usr/bin/sample <pid> 5 -file /tmp/frankenterm-<pid>.sample.txt
heap <pid> > /tmp/frankenterm-<pid>.heap.txt
```

The closing diagnosis must classify resident bytes through the v1
`residency_buckets` rows as `rust_heap`, `mmap_file_backed`,
`sqlite_page_cache`, `graphics_media`, `scrollback_cache`, `child_processes`, or
`unknown`. Cross-check `domains.rss_residency`, `domains.storage_io`,
`domains.action_receipts`, `action_receipts`, and `artifact_paths` before naming
an incident root cause. Non-zero `unknown` is an investigation result, not a
bucket to hide in a leak summary.

## Minimal Example

```json
{
  "schema_version": 1,
  "contract_id": "ft.resource_pressure_cockpit.v1",
  "generated_at_ms": 1778359200000,
  "source": "runtime_telemetry.swarm_resource_cockpit",
  "status": "watch",
  "proof_gate": "pressured",
  "evidence_state": "mixed",
  "summary": "memory tier pressure elevated; storage IO green",
  "next_operator_move": "hold pane fanout and retain doctor plus events artifacts",
  "run_identity": {
    "run_id": "capacity-20260509T180000Z",
    "evidence_level": "remote_reduced",
    "git_head": "unknown",
    "artifact_paths": [
      "tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json"
    ],
    "hardware_predicate": {
      "logical_cpus": 16,
      "memory_gib": 64,
      "target_class": false,
      "proof_status": "skipped_not_proven"
    }
  },
  "domains": {
    "memory": {
      "name": "memory",
      "evidence_state": "measured",
      "pressure_tier": "elevated",
      "summary": "warm compressed tier over budget",
      "operator_action": "evict_warm_to_cold",
      "reason_codes": ["resource.memory.tier_pressure"]
    },
    "rss_residency": {
      "name": "rss_residency",
      "evidence_state": "unavailable",
      "pressure_tier": "unknown",
      "summary": "residency classifier not attached",
      "operator_action": "capture_diagnostic_bundle",
      "reason_codes": ["resource.telemetry.unavailable"]
    },
    "pane_budget": {
      "name": "pane_budget",
      "evidence_state": "measured",
      "pressure_tier": "normal",
      "summary": "pane budgets within limits",
      "operator_action": "none",
      "reason_codes": ["resource.proof.healthy"]
    },
    "queue_backpressure": {
      "name": "queue_backpressure",
      "evidence_state": "measured",
      "pressure_tier": "green",
      "summary": "queues below warning thresholds",
      "operator_action": "none",
      "reason_codes": ["resource.proof.healthy"]
    },
    "storage_io": {
      "name": "storage_io",
      "evidence_state": "measured",
      "pressure_tier": "green",
      "summary": "storage queue is green",
      "operator_action": "none",
      "reason_codes": ["resource.proof.healthy"]
    },
    "worker_pool": {
      "name": "worker_pool",
      "evidence_state": "simulated",
      "pressure_tier": "unknown",
      "summary": "fixture worker pool only",
      "operator_action": "run_target_worker_probe",
      "reason_codes": ["resource.telemetry.simulated"]
    },
    "capacity_admission": {
      "name": "capacity_admission",
      "evidence_state": "measured",
      "pressure_tier": "elevated",
      "summary": "planned degrade under watch gate",
      "operator_action": "hold_fanout",
      "reason_codes": ["admission.degrade.pressure"]
    },
    "resource_admission": {
      "name": "resource_admission",
      "evidence_state": "measured",
      "pressure_tier": "elevated",
      "summary": "resource admission degraded noncritical work",
      "operator_action": "degrade",
      "reason_codes": ["memory_tier_pressure"]
    },
    "action_receipts": {
      "name": "action_receipts",
      "evidence_state": "measured",
      "pressure_tier": "green",
      "summary": "no failed receipts",
      "operator_action": "none",
      "reason_codes": ["action_receipt.applied"]
    }
  },
  "memory_tiers": [],
  "residency_buckets": [
    {
      "bucket": "sqlite_page_cache",
      "bucket_name": "SQLite page cache",
      "evidence_state": "unavailable",
      "bytes": null,
      "confidence": 0,
      "dominant": false,
      "reason_codes": ["resource.telemetry.unavailable"]
    }
  ],
  "queue_backpressure": [],
  "admission_decisions": [],
  "action_receipts": [
    {
      "receipt_id": "receipt-20260509T180000Z-001",
      "action": "hold_fanout",
      "target_domain": "capacity_admission",
      "requested_at_ms": 1778359200000,
      "completed_at_ms": 1778359201000,
      "status": "succeeded",
      "dry_run": false,
      "policy_decision": "allow",
      "evidence_state": "measured",
      "reason_codes": ["action_receipt.applied"],
      "artifact_paths": [
        "tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json"
      ]
    }
  ],
  "mitigation_history": [],
  "drilldowns": [
    {
      "subject": "rss_residency",
      "reason_code": "resource.telemetry.unavailable",
      "detail": "classifier is not implemented yet"
    }
  ],
  "artifact_paths": [
    "tests/e2e/artifacts/goal-line/ft-rz0eb.4/resource_cockpit_conformance/20260510T125418Z/summary.json"
  ]
}
```
