# Safe auto-tuning contract and knob registry (ft-luq3w.1)

## Status

Contract for `ft-luq3w.1`. This document defines the initial safe
auto-tuning surface for `ft-luq3w` before any live controller changes are
made.

This is not an implementation. It is the guardrail document that future
implementation beads must follow so FrankenTerm can improve throughput and
latency on large agent swarms without hiding regressions, inventing
unsupported 64-core claims, or turning normal operator configuration into an
unbounded adaptive system.

## Current Ground Truth

Existing code already has three relevant surfaces:

| Surface | Current anchor | Contract relevance |
| --- | --- | --- |
| Operator tuning config | `crates/frankenterm-core-config-types/src/tuning_config.rs`, `docs/tuning-reference.md` | Static `ft.toml` tuning keys with defaults, validation, and recommended ranges. This is the initial source for safe knob bounds. |
| Legacy proportional tuner | `crates/frankenterm-core/src/auto_tune.rs` | Library-level `AutoTuner` with `TunableParams`, `AutoTuneConfig`, hysteresis, pinned params, and adjustment logs. This is useful precedent, but it is not by itself a live fleet policy. |
| Resource cockpit and capacity telemetry | `crates/frankenterm-core/src/runtime_telemetry.rs` | Operator-facing proof surface through `SwarmResourceCockpitSnapshot`, capacity stages, admission decisions, proof gates, and high-scale evidence artifacts. |

Important existing behavior:

- `TuningConfig` is immutable after load and defaults to historical
  hard-coded behavior.
- `AutoTuner` adjusts five parameters in isolation:
  `poll_interval_ms`, `scrollback_lines`, `snapshot_interval_secs`,
  `pool_size`, and `backpressure_threshold`.
- `AutoTuneConfig::default()` currently has `enabled: true`; future live
  fleet wiring must not treat that library default as permission to mutate
  runtime behavior.
- The capacity controller already has opt-in and dry-run fields through
  `SwarmCapacityAdmissionControllerConfig`.
- High-scale proof claims must stay distinct from local or reduced-mode
  smoke, as described by `docs/high-core-swarm-runbook.md` and
  `docs/ft-xbnl0-verification-contract.md`.

## Non-Goals

- Do not hot-reload arbitrary `TuningConfig` fields as part of this contract.
- Do not allow auto-tuning of policy authorization, audit retention,
  filesystem paths, wire-format limits, secret redaction, or security gates.
- Do not make the legacy `AutoTuner` live by default.
- Do not claim 64-core / 256 GiB benefit from synthetic or undersized runs.
- Do not tune based on missing, stale, unverifiable, or redacted-away
  telemetry.
- Do not let a successful queue/admission decision stand in for durable
  completion, search freshness, or audit persistence.

## Controller Modes

Every implementation bead must preserve these modes. A tuning engine may only
mutate live knobs in `steady_state` or `rollback`.

| Mode | Mutates live knobs | Purpose | Required operator signal |
| --- | --- | --- | --- |
| `disabled` | No | Compile-time and runtime off state. | `auto_tuning.mode=disabled`; no candidate evaluation. |
| `observe` | No | Collect telemetry and emit would-have-tuned decisions. | Decision log with `would_apply=false`. |
| `canary` | Only canary-scoped instances | Evaluate one bounded candidate against a small scope. | Candidate id, canary scope, baseline hash, rollback condition. |
| `exploration` | Bounded and temporary | Try one registry-approved step while tracking regression metrics. | Expiry tick, max step count, and rollback metric. |
| `steady_state` | Yes | Keep a previously proven setting inside a narrow range. | Current profile, proof level, and drift checks. |
| `rollback` | Yes, toward previous safe value only | Undo a candidate or steady-state setting after regression. | Trigger reason and restored value. |
| `cooldown` | No | Pause adaptation after rollback, drift, missing telemetry, or operator pause. | Cooldown deadline and last unsafe reason. |

Mode transitions are one-way unless the new mode has fresh evidence:

```text
disabled -> observe -> canary -> exploration -> steady_state
             ^           |          |              |
             |           v          v              v
             +-------- rollback <- cooldown <------+
```

`rollback` must restore the last known safe value before any new candidate is
considered. `cooldown` must clear only after telemetry is fresh and the
rollback metric has returned inside budget.

## Initial Safe Knob Registry

The registry below is the complete initial set. Future implementation beads
must update this document before tuning anything else.

| Knob id | Owner | Current source | Default | Hard bounds | Initial step | Safety metric | Rollback metric | Gate |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `runtime.output_coalesce_window_ms` | Runtime ingest/capture | `RuntimeTuning::output_coalesce_window_ms` | `50 ms` | `5..=200 ms` | `+/-25 ms` | ingest p95 latency, segment flush count, memory bytes buffered | p99 ingest latency regresses over baseline or buffer bytes exceed budget | profile-gated |
| `runtime.output_coalesce_max_delay_ms` | Runtime ingest/capture | `RuntimeTuning::output_coalesce_max_delay_ms` | `200 ms` | `window..=750 ms` | `+/-50 ms` | flush boundedness, p99 pane freshness | pane freshness p99 exceeds SLO or max delay falls below window | profile-gated |
| `runtime.output_coalesce_max_bytes` | Runtime ingest/storage | `RuntimeTuning::output_coalesce_max_bytes` | `262144 bytes` | `4096..=1048576 bytes` | x2 or /2, one step only | storage write service p95, queue depth, memory headroom | storage write p99 or queue depth over budget | profile-gated |
| `runtime.telemetry_percentile_window` | Runtime telemetry | `RuntimeTuning::telemetry_percentile_window` | `1024 samples` | `256..=4096 samples` | x2 or /2 | telemetry memory cap, stage sample sufficiency | telemetry cap exceeds budget or sample writes contend with hot path | observe-first |
| `runtime.cursor_snapshot_memory_warn_bytes` | Runtime memory tiering | `RuntimeTuning::cursor_snapshot_memory_warn_bytes` | `67108864 bytes` | `33554432..=536870912 bytes` | x2 or /2 | memory pressure tier, hot resident bytes | memory pressure reaches critical/emergency or retained cursor bytes breach budget | profile-gated |
| `backpressure.warn_ratio` | Backpressure/capacity | `BackpressureTuning::warn_ratio` | `0.75` | `0.10..=0.99` | `+/-0.05` | false positive warning rate, queue saturation rate | queue saturation rises or warnings arrive after capacity action | canary-first |
| `snapshot.trigger_bridge_tick_secs` | Snapshot maintenance | `SnapshotTuning::trigger_bridge_tick_secs` | `30 s` | `5..=120 s` | `+/-15 s` | trigger latency, idle CPU | missed trigger SLA or idle CPU over baseline | observe-first |
| `snapshot.memory_trigger_cooldown_secs` | Snapshot memory mitigation | `SnapshotTuning::memory_trigger_cooldown_secs` | `120 s` | `60..=600 s` | `+/-60 s` | repeated memory-trigger rate, memory recovery time | memory remains over budget or repeated snapshots create IO pressure | canary-first |
| `ingest.max_persist_segment_bytes` | Ingest/storage | `IngestTuning::max_persist_segment_bytes` | `65536 bytes` | `32768..=262144 bytes` | x2 or /2 | storage write p95, segment count, search freshness lag | write p99, search lag, or memory queue exceeds baseline budget | canary-first |
| `patterns.max_seen_keys` | Pattern detection | `PatternsTuning::max_seen_keys` | `1000 entries` | `100..=64000 entries` | x2 or /2 | dedupe hit rate, pattern CPU, memory footprint | pattern CPU or memory grows without lower duplicate events | observe-first |
| `patterns.max_tail_size_bytes` | Pattern detection | `PatternsTuning::max_tail_size_bytes` | `2048 bytes` | `256..=16384 bytes` | x2 or /2 | detection recall proxies, regex CPU, retained tail bytes | CPU or memory grows without recovered detections | observe-first |
| `patterns.bloom_false_positive_rate` | Pattern detection | `PatternsTuning::bloom_false_positive_rate` | `0.01` | `0.001..=0.2` | one documented tier | regex evaluations, Bloom memory | regex CPU or false-positive work rises over baseline | observe-first |
| `policy.max_tracked_panes` | Policy/rate limiting | `PolicyTuning::max_tracked_panes` | `256 panes` | `32..=8192 panes` | x2 or /2 | rate-limit amnesia count, policy memory | policy memory exceeds budget or evictions remain high | canary-first |
| `policy.max_events_per_pane` | Policy/rate limiting | `PolicyTuning::max_events_per_pane` | `64 events` | `8..=512 events` | x2 or /2 | rate-limit accuracy, policy memory | memory exceeds budget or enforcement churn rises | canary-first |
| `policy.cost_tracker_max_panes` | Policy/cost tracking | `PolicyTuning::cost_tracker_max_panes` | `512 panes` | `128..=8192 panes` | x2 or /2 | cost tracker eviction count, memory | memory exceeds budget or tracker remains evicting | canary-first |
| `web.stream_default_max_hz` | Web/API streaming | `WebTuning::stream_default_max_hz` | `50 Hz` | `1..=250 Hz` | one profile tier | SSE lag, CPU fanout, client backlog | CPU or stream backlog rises over baseline | canary-first |
| `web.stream_scan_limit` | Web/API streaming | `WebTuning::stream_scan_limit` | `256 rows` | `1..=1024 rows` | x2 or /2 | catch-up latency, scan CPU | scan CPU or request latency regresses | canary-first |
| `workflows.cass_*_timeout_secs` | Workflow/CASS handlers | `CassQueryConfig::timeout_secs` | `6..8 s` | `4..=15 s` | `+/-2 s` | CASS success rate, workflow latency | workflow p99 or cancellation rate regresses | observe-first |
| `workflows.*_cooldown_ms` | Workflow handlers | `WorkflowsTuning` cooldown fields | `180000..900000 ms` | `60000..=1800000 ms` | x2 or /2 | duplicate automation rate, recovery latency | repeated intervention or delayed recovery exceeds baseline | observe-first |
| `search.tantivy_writer_memory_bytes` | Search/indexing | `SearchTuning::tantivy_writer_memory_bytes` | `50000000 bytes` | `10485760..=268435456 bytes` | x2 or /2 | indexing throughput, search freshness, memory pressure | memory pressure or search lag regresses | canary-first |
| `ipc.accept_poll_interval_ms` | IPC | `IpcTuning::accept_poll_interval_ms` | `100 ms` | `10..=250 ms` | `+/-25 ms` | accept latency, idle CPU | accept p99 or idle CPU regresses | observe-first |
| `capacity.queue_defer_threshold` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `16` | `1..=1024` and `defer <= throttle <= shed` | x2 or /2 | admission queue pressure, defer count, completed work | capacity action becomes noisier or backlog grows | canary-first |
| `capacity.backlog_defer_threshold` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `64` | `1..=4096` and `defer <= throttle <= shed` | x2 or /2 | backlog pressure, defer count, completed work | backlog grows or defers arrive too late | canary-first |
| `capacity.throttle_queue_depth` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `64` | `queue_defer..=shed` | x2 or /2 | throttle rate, capture freshness | capture freshness or completed work regresses | canary-first |
| `capacity.throttle_backlog_depth` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `256` | `backlog_defer..=shed` | x2 or /2 | throttle rate, backlog drain time | backlog drain worsens or throttles too early | canary-first |
| `capacity.shed_queue_depth` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `256` | `throttle..=4096` | x2 or /2 | optional shed count, queue saturation | mandatory work is shed or queue still saturates | canary-first |
| `capacity.shed_backlog_depth` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `1024` | `throttle..=16384` | x2 or /2 | optional shed count, backlog saturation | mandatory work is shed or backlog still saturates | canary-first |
| `capacity.default_retry_after_secs` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `5 s` | `1..=max_retry_after_secs` | `+/-5 s` | retry storm rate, completion latency | herd waves, retries, or completion latency regress | observe-first |
| `capacity.cooldown_secs` | Capacity admission | `SwarmCapacityAdmissionControllerConfig` | `30 s` | `0..=3600 s` | `+/-30 s` | oscillation count, recovery time | oscillation returns or recovery is delayed | observe-first |

Knobs outside this registry are unsupported for auto-tuning even if they are
operator-configurable in `ft.toml`.

## Telemetry Freshness and Trust Rules

A candidate evaluation must refuse to tune unless all required inputs are
fresh and trusted:

| Input | Freshness rule | Required use |
| --- | --- | --- |
| `SwarmResourceCockpitSnapshot` | Same run, schema version recognized, proof gate not `skipped_proof` for live mutation. | Capacity, memory, latency, and resource-admission decisions. |
| Stage latency samples | Enough samples for the target stage in the current window. | Runtime, ingest, storage, IPC, web, and workflow latency rollback metrics. |
| Memory tier summary | Generated from the same capacity summary as the candidate. | Cursor, search writer memory, and any memory-expanding knob. |
| Search freshness or indexing lag | Same database/run id as the candidate workload. | Search writer and ingest segment-size decisions. |
| Policy/audit counters | Same process generation; no missing audit table diagnostics. | Policy tracker and audit-sensitive cooldown decisions. |
| High-scale hardware predicate | `proven_predicate_met` from `ft doctor --json`. | Any claim that a setting is proven for 64-core / 256 GiB operation. |

Missing telemetry is not neutral. Missing telemetry forces `observe`,
`cooldown`, or `rollback`, depending on whether a live candidate is active.

## Candidate Decision Record

Every proposed or applied tuning decision must be machine-readable:

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | u32 | Decision schema version. |
| `decision_id` | string | Stable hash over candidate, baseline, mode, and evidence pointers. |
| `mode` | string | One of the controller modes above. |
| `knob_id` | string | Registry id. |
| `owner` | string | Owning subsystem. |
| `old_value` | JSON scalar | Previous value. |
| `candidate_value` | JSON scalar | Proposed or applied value. |
| `step` | JSON scalar | Step size and direction. |
| `would_apply` | bool | False for `observe`. |
| `applied` | bool | True only after the live mutation succeeds. |
| `baseline_hash` | string | Hash of the baseline metrics/config snapshot. |
| `evidence_level` | string | `local_reduced`, `remote_reduced`, `target_hardware`, or `skipped_not_proven`. |
| `proof_gate` | string | Resource cockpit proof gate. |
| `safety_metric` | string | Primary safety metric from registry. |
| `rollback_metric` | string | Primary rollback metric from registry. |
| `rollback_condition` | string | Concrete predicate that returns to the prior value. |
| `cooldown_until_ms` | optional u64 | Required after rollback or missing telemetry. |
| `reason_code` | string | Stable reason such as `auto_tune.observe.stale_telemetry`. |
| `artifact_paths` | string array | Retained proof logs, diagnostics, or replay artifacts. |

Decision records must redact pane content and secrets. They may include stable
ids, hashes, metric aggregates, and artifact paths.

## Rollback and Cooldown Rules

Rollback is mandatory when any of these conditions hold:

1. The registry rollback metric breaches its budget for two consecutive
   windows, or for one window if the proof gate is `degraded`.
2. A candidate increases pressure in a different subsystem, for example lower
   search lag at the cost of critical memory pressure.
3. Telemetry becomes stale or unavailable while a live candidate is active.
4. A required durable or audit path reports fail-closed pressure after the
   candidate changed.
5. The operator disables the profile or pins the knob.

Rollback must:

- restore the previous safe value,
- record the trigger metric and old/candidate/restored values,
- enter `cooldown`,
- refuse a second attempt on the same knob until cooldown expires and
  telemetry is fresh,
- preserve the failed candidate in diagnostics so it is not retried blindly.

Cooldown defaults to at least three evaluation windows. Capacity-admission
cooldown must also respect `SwarmCapacityAdmissionControllerConfig` cooldown
semantics.

## Combination Guardrails

The first implementation must explore at most one primary knob at a time.
Later multi-knob candidates may be added only after this document is updated.

Forbidden initial combinations:

| Combination | Reason |
| --- | --- |
| Lower `backpressure.warn_ratio` while lowering capacity defer thresholds. | Double-counts queue pressure and can over-defer useful work. |
| Increase `output_coalesce_max_bytes` while increasing `ingest.max_persist_segment_bytes`. | Can create larger memory and SQLite bursts without isolating cause. |
| Increase `tantivy_writer_memory_bytes` while increasing pattern tail/dedup memory. | Can hide memory pressure inside multiple caches. |
| Lower IPC accept interval while raising web stream rate. | Can convert operator polling into CPU fanout. |
| Raise workflow CASS timeouts while lowering workflow cooldowns. | Can make automation both slower and more frequent. |
| Tune capacity shed thresholds while any audit or policy-denial write is failing. | Can hide safety failures behind resource pressure. |
| Tune any search/indexing knob while storage IO scheduler verdicts are missing. | Search lag and IO pressure cannot be separated without scheduler metrics. |

## Proof Levels

Use these labels consistently in decision records, Beads comments, and docs:

| Evidence level | May mutate live knobs | May claim high-scale benefit | Meaning |
| --- | --- | --- | --- |
| `local_reduced` | No, except developer-only canary harnesses | No | Local or small-worker proof of logic, schema, and rollback behavior. |
| `remote_reduced` | Canary only | No | Remote `rch` proof reached Cargo/tests, but hardware predicate is not target-class. |
| `target_hardware` | Yes, if other gates pass | Yes | Host satisfies 64-core / 256 GiB predicate and artifacts are retained. |
| `skipped_not_proven` | No | No | Required proof was not available, stale, or hardware predicate failed. |

Any future README or operator doc wording must distinguish "supported by local
reduced proof" from "proven on target-class hardware."

## Required Implementation Beads

Future beads under `ft-luq3w` should implement the contract in this order:

1. A registry data model that serializes the table above and rejects
   unregistered knobs.
2. A decision/evidence record type with stable hashes and redaction-safe fields.
3. A bounded exploration controller with explicit modes, pinned knobs, and
   single-knob candidate generation.
4. Rollback and cooldown state with property tests for monotonic safety.
5. Cockpit integration so operators can inspect current mode, candidate,
   rollback reason, and proof level.
6. Replay tests proving a fixed trace either improves or preserves the chosen
   metric and rolls back when the metric regresses.
7. Target-hardware runbook wiring that can report `skipped_not_proven` instead
   of exaggerating local smoke.

## Test Plan

Implementation must include:

- Unit tests that reject unknown knob ids.
- Unit tests for min/max/step validation on every registry row.
- Property tests that a candidate never steps outside hard bounds.
- Property tests that rollback always returns to the prior safe value.
- Tests that missing telemetry forces `observe`, `cooldown`, or `rollback`.
- Tests that high-scale proof remains `skipped_not_proven` when hardware
  predicates are missing.
- Replay tests with structured logs showing candidate, metric, decision, and
  rollback state.
- At least one negative test for each forbidden combination class.

Cargo proof must run through `rch`. If the RCH timeout-wrapper bead remains
blocked, non-Cargo checks are hygiene only and the Beads closeout must say so.
