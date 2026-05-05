# Storage IO scheduler contract (ft-1grhq.1)

## Status

Contract for `ft-1grhq.1`. This document defines the work classes,
admission outcomes, ordering rules, metrics, and proof obligations that
the implementation beads under `ft-1grhq` must preserve.

This is not an implementation. It is the scheduler boundary that makes
`ft-1grhq.2` through `ft-1grhq.6` implementable without reinterpreting
storage semantics.

## Current Ground Truth

The current storage path is a single bounded writer channel plus a
dedicated SQLite writer thread:

```text
caller
  -> StorageHandle async API
  -> bounded mpsc<WriteCommand>
  -> writer_loop()
  -> dispatch_write_command()
  -> SQLite WAL, FTS triggers, optional mmap mirror
```

Important existing surfaces:

| Surface | Current anchor | Scheduler relevance |
| --- | --- | --- |
| Segment persistence | `StorageHandle::append_segment_with_cx`, `WriteCommand::AppendSegment`, `append_segment_sync` | Highest-volume durable write path. Per-pane `seq` ordering is authoritative. |
| Gap persistence | `WriteCommand::RecordGap`, `record_gap_backend` | Must remain fail-closed enough that skipped output becomes explicit. |
| Event and workflow writes | `WriteCommand::RecordEvent`, `WriteCommand::InsertStepLog`, workflow/action plan variants | Can be deferred briefly, but errors must stay visible to the caller. |
| Audit writes | `WriteCommand::RecordAuditAction`, `WriteCommand::RecordPolicyDenialAudit`, `record_policy_denial_audit_blocking` | Audit and policy-denial writes must never be silently shed. |
| FTS catch-up and rebuild | `StorageHandle::sync_fts_with_cx`, `sync_fts_for_pane`, `full_fts_rebuild_sync` | Background-heavy IO. May lag, but lag must be measurable and bounded by policy. |
| Search indexing pipeline | `search/indexing_pipeline.rs`, `SearchIndex::ingest_documents_detailed` | Separate index state uses batching, pending docs, rate limits, and freshness stats. |
| Cold-tier writes and reads | `scrollback_cold_tier.rs`, `cold_tier_pipeline_driver.rs`, `scrollback_tiers.rs` | Bulk IO with retry/backoff. Good candidate for low-priority scheduling and chaos injection. |
| Resource cockpit and capacity telemetry | `SwarmCapacityStage::StorageWrite`, `SwarmResourceCockpitSnapshot` | Operator-facing pressure and proof surface for scheduler decisions. |
| Generic metrics | `StorageHandle::write_queue_depth`, `metrics.rs` | Existing queue depth signal, but not enough for class-specific IO pressure. |

`StorageConfig::write_queue_size` is the current aggregate queue bound.
`StorageConfig::defer_fts_triggers` is the current opt-out from immediate
SQLite FTS triggers; when enabled, callers must periodically run FTS
sync to catch the index up. The scheduler must treat that as an explicit
freshness tradeoff, not as durable search success.

## Non-Goals

- Do not replace SQLite WAL or the dedicated writer thread as part of
  `ft-1grhq.1`.
- Do not move the storage monolith or split `storage.rs`; that belongs
  to storage split beads such as `ft-dn2tu`.
- Do not introduce direct `tokio` usage. Implementation must use the
  project `runtime_async` surface.
- Do not batch multiple caller-visible writes in a transaction unless
  their caller replies are delayed until the transaction result is known.
  The current writer-loop comment is correct: a caller must not receive
  success before a later commit can fail.
- Do not claim queued work is durable. Queued, deferred, degraded, and
  durable-complete states are separate states.

## Scheduler Responsibility Boundary

The scheduler owns admission and service ordering for IO work that can
compete for disk, WAL, FTS, mmap mirror, cold-tier files, or storage
background maintenance. It decides whether work is accepted immediately,
batched, deferred, degraded, shed, or failed closed.

The scheduler does not own:

- serialization formats for stored rows or cold-tier files,
- policy authorization decisions before audit rows are created,
- redaction rules,
- search ranking,
- pane discovery,
- runtime admission for non-storage CPU work except through existing
  resource/capacity summaries.

The scheduler API should be usable before dispatching a `WriteCommand`
and by background maintenance loops. A caller must be able to ask:

```text
admit(work_item, observed_pressure) -> AdmissionVerdict
```

The verdict must say whether the caller may proceed, must wait/retry,
should use a degraded path, or must fail closed.

## Work Classes

Use a small fixed class set. These names are deliberately stable so
metrics, cockpit rows, and chaos tests can key on them.

| Class | Examples | Ordering | Default priority | Shed policy |
| --- | --- | --- | --- | --- |
| `pane_segment_durable` | `AppendSegment`, redactor flush before gaps | Strict per pane by `seq`; no cross-pane ordering requirement | High | Never shed silently. Under pressure, degrade to explicit gap or fail closed. |
| `gap_and_continuity` | `RecordGap`, continuity metadata | Strict per pane relative to segment stream | Highest | Never shed. A failed gap write is a fail-closed diagnostic. |
| `policy_audit` | `RecordPolicyDenialAudit`, `RecordAuditAction` for policy decisions | Preserve per-correlation order where known | Highest | Never shed. Blocking fallback is allowed only if explicitly marked. |
| `workflow_event` | events, workflow step logs, action-plan writes | Preserve workflow/correlation order; cross-workflow fairness allowed | Medium | Defer first; shed only optional/duplicate diagnostics with structured loss record. |
| `fts_incremental` | `sync_fts_for_pane`, deferred FTS catch-up | Per pane high-water mark order | Low | Defer under pressure. Never report search freshness beyond last committed progress. |
| `fts_rebuild` | `full_fts_rebuild_sync`, corrupt-index rebuild | Rebuild-global; mutually exclusive with incremental catch-up for same DB | Low | Pause/defer. If corruption requires rebuild, surface `search_unhealthy`. |
| `search_index_state` | `SearchIndex::flush_now`, index state maintenance | Index-local consistency | Low | Defer when source data remains durable. Do not drop accepted docs without count. |
| `cold_tier_write` | compression/redaction/encryption/persist/index cold-tier pipeline | Per chunk state-machine order | Low | Retry/defer first; shed only if chunk is already reconstructible or explicitly marked unavailable. |
| `cold_tier_read` | `ColdTierRetriever`, hydration for old scrollback | User-visible reads outrank background writes | Medium | Defer with retry-after; fail as `cold_storage_unavailable` if exhausted. |
| `storage_maintenance` | WAL checkpoint, vacuum, optimize, retention purge | Maintenance-defined | Lowest | Defer freely unless health gate says maintenance is required for safety. |

Implementation may add sub-classes, but they must roll up to one of
these classes for metrics and chaos verdicts.

## Admission Outcomes

Every admission decision returns one of these outcomes:

| Outcome | Meaning | Caller contract | Operator reason-code prefix |
| --- | --- | --- | --- |
| `admit` | Caller may enqueue/run now. | Work still succeeds only after the underlying IO completes. | `storage_io.admit.*` |
| `batch` | Caller may enqueue into a bounded class batch. | Reply cannot report durable success until the batch item completes. | `storage_io.batch.*` |
| `defer` | Caller should retry later or leave work queued in a bounded delayed queue. | Include retry-after or wake condition. | `storage_io.defer.*` |
| `degrade` | Caller should use an explicitly weaker but safe path. | Degraded path must emit a diagnostic or gap. | `storage_io.degrade.*` |
| `shed` | Optional work is dropped before execution. | Only allowed for non-authoritative work; must increment loss counters. | `storage_io.shed.*` |
| `fail_closed` | Work cannot safely continue. | Return an error or durable diagnostic; do not pretend success. | `storage_io.fail_closed.*` |

Required reason-code suffixes:

| Suffix | Use |
| --- | --- |
| `queue_full` | Aggregate or class queue reached hard bound. |
| `class_budget_exhausted` | Class-specific budget reached before aggregate bound. |
| `oldest_age_exceeded` | Oldest queued item breached latency SLA. |
| `io_error` | Underlying write/read failed. |
| `search_freshness_lag` | FTS or search index is behind source data. |
| `cold_tier_unavailable` | Cold-tier file or retriever cannot serve the request. |
| `audit_required` | Work must fail closed because an audit row cannot be persisted. |
| `operator_disabled` | Scheduler or a class is disabled by config. |
| `chaos_injection` | Chaos harness intentionally injected a fault. |

## Queue and Fairness Rules

The scheduler must maintain:

1. An aggregate pending-byte budget across all IO classes.
2. A bounded queue per work class.
3. Oldest queued age per class.
4. At least one starvation prevention rule for low-priority classes.
5. A strict no-silent-loss rule for durable segments, gaps, and audit.

Fairness policy:

- `gap_and_continuity` and `policy_audit` outrank all other classes.
- `pane_segment_durable` outranks search, cold-tier background work, and
  maintenance.
- `cold_tier_read` outranks `cold_tier_write` because it serves an
  operator/user read.
- Background classes (`fts_incremental`, `fts_rebuild`,
  `search_index_state`, `cold_tier_write`, `storage_maintenance`) must
  make progress eventually when high-priority pressure clears.
- No class may hold the aggregate budget forever. A class that repeatedly
  defers must expose oldest-age and defer-count metrics.

Ordering policy:

- Segment and gap work is ordered per pane. Cross-pane batching is allowed
  only if each pane's relative order is preserved.
- Policy/audit work is ordered per correlation ID when the ID is present.
- FTS incremental work is ordered per pane by `seq` and progress row.
- Cold-tier write work follows the cold-tier pipeline state machine:
  compress/redact/encrypt/persist/index steps cannot be reordered.
- Maintenance work must not overtake a required durability write if doing
  so would make the durability write fail spuriously.

## Durability and Audit Invariants

These invariants are mandatory acceptance criteria for implementation:

1. `pane_segment_durable` success means the segment write completed, not
   merely that the segment was accepted by a scheduler queue.
2. If segment content is dropped or skipped because of pressure, the
   system emits an explicit gap or fail-closed error.
3. Policy-denial audit writes are not optional. If the scheduler cannot
   admit them through the normal writer path, it must either use a named
   blocking fallback or return a fail-closed diagnostic.
4. FTS/search freshness is not the same as segment durability. Deferred
   indexing must expose lag and last committed progress.
5. Cold-tier metadata cannot claim a chunk is on disk until the pipeline
   reaches the persist/index state that the cold-tier substrate defines.
6. A caller-facing oneshot reply cannot report success before the work's
   underlying durability condition is met.
7. Every `degrade`, `shed`, and `fail_closed` decision is observable in
   structured telemetry.

## Metrics and Verdict Fields

The scheduler snapshot must be machine-readable and stable enough for the
resource cockpit and `ft-lmg3g.4` chaos tests.

Required per-class fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `class` | string | One of the stable class labels above. |
| `queue_depth` | u64 | Number of pending work items. |
| `queue_capacity` | u64 | Hard item cap for the class. |
| `bytes_pending` | u64 | Estimated pending bytes. |
| `bytes_capacity` | u64 | Class byte budget. |
| `oldest_queued_age_ms` | optional u64 | Age of oldest pending item. |
| `admitted_total` | u64 | Count of admitted items. |
| `batched_total` | u64 | Count of batched items. |
| `deferred_total` | u64 | Count of deferred items. |
| `degraded_total` | u64 | Count of degraded items. |
| `shed_total` | u64 | Count of shed items. |
| `fail_closed_total` | u64 | Count of fail-closed decisions. |
| `write_error_total` | u64 | Underlying write errors. |
| `last_reason_code` | optional string | Last decision reason for the class. |

Required global fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | u32 | Snapshot schema version. |
| `aggregate_queue_depth` | u64 | Total queued items. |
| `aggregate_bytes_pending` | u64 | Total pending bytes. |
| `io_pressure_tier` | string | `green`, `yellow`, `red`, or `black`. |
| `io_pressure_reason` | string | Stable reason code. |
| `search_lag_segments` | u64 | Source segments not yet visible in FTS/search. |
| `search_lag_oldest_age_ms` | optional u64 | Age of oldest unindexed segment. |
| `hydration_lag_pages` | u64 | Cold pages/chunks waiting for hydration or writeback. |
| `audit_fail_closed_total` | u64 | Audit-critical fail-closed count. |
| `durability_pending_total` | u64 | Items accepted but not durably complete. |

The resource cockpit should display storage IO pressure as a separate
drilldown from CPU and memory pressure. A storage defer/degrade decision
must not be described as CPU or memory pressure unless those inputs
actually caused the decision.

`ft-1grhq.5` exposes this as `.swarm_capacity.resource_cockpit.storage_io`
using the scheduler's `StorageIoOperatorSummary`. The compact cockpit rows and
drilldowns key storage-specific pressure with `subject=storage_io` plus stable
`storage_io.*` reason codes, so chaos/conformance tests do not need to parse
operator prose.

## Chaos Harness Contract for ft-lmg3g.4

`ft-lmg3g.4` should be able to inject and observe at least these faults:

| Injection | Expected scheduler behavior | Required observable verdict |
| --- | --- | --- |
| Slow segment writes | `pane_segment_durable` queue grows, then defers/degrades before unbounded memory growth. | `storage_io.defer.queue_full` or `storage_io.degrade.oldest_age_exceeded`; no false durable success. |
| SQLite insert error on segment | Segment write returns error or explicit gap path. | `storage_io.fail_closed.io_error`; `write_error_total` increments. |
| FTS catch-up stall | Segment durability continues; search freshness lag grows. | `storage_io.defer.search_freshness_lag`; `search_lag_segments` grows. |
| FTS rebuild corruption path | Rebuild is exclusive and progress is bounded. | `search_unhealthy` plus rebuild decision/log row. |
| Cold-tier write retry exhaustion | Chunk remains not-on-disk or unavailable, with retry summary. | `storage_io.fail_closed.cold_tier_unavailable` or explicit unavailable state. |
| Cold-tier read stall | User-visible read defers with retry-after, then fails clearly. | `storage_io.defer.cold_tier_unavailable` followed by bounded fail result if exhausted. |
| Audit writer failure | Policy/audit work fails closed or uses named blocking fallback. | `storage_io.fail_closed.audit_required`; no silent allow. |
| Maintenance backlog | Maintenance defers behind durable work. | `storage_io.defer.class_budget_exhausted`; no segment/audit starvation. |

The chaos verdict should include:

```text
scenario_id
injected_fault
affected_class
observed_outcomes[]
queue_depth_peak
oldest_queued_age_peak_ms
bytes_pending_peak
durable_success_count
false_success_count
shed_count
fail_closed_count
search_lag_segments_peak
hydration_lag_pages_peak
operator_reason_codes[]
```

`false_success_count` must be zero for a passing scenario.

## Unit Test Plan for ft-1grhq.2

Implementation should add focused tests before wider E2E proof:

1. Per-class queue bounds: a class refuses or defers once its item or
   byte budget is exhausted.
2. Aggregate bounds: aggregate byte pressure can defer a class even when
   that class has local capacity.
3. Per-pane ordering: segment and gap items for the same pane remain in
   order across batching and dispatch.
4. Cross-class priority: audit and gap work outrank background FTS and
   maintenance work under pressure.
5. Starvation prevention: low-priority background work progresses after
   high-priority pressure clears.
6. Fail-closed audit: audit-critical work cannot be shed or reported as
   success when its write fails.
7. Deferred search freshness: segment writes can complete while FTS lag
   is reported accurately.
8. Cold-tier retry exhaustion: retryable cold-tier failures do not become
   durable success after retry budget is exhausted.
9. Reason-code stability: every non-`admit` outcome has a stable
   `storage_io.*` reason code.
10. Snapshot schema: required metrics serialize with stable field names.

## Implementation Sequence

1. `ft-1grhq.2`: implement scheduler data types, bounded admission, class
   queues, outcomes, ordering tests, and snapshot metrics.
2. `ft-1grhq.3`: route segment persistence plus audit/event writes through
   the scheduler. Keep success tied to real write completion.
3. `ft-1grhq.4`: connect cold-tier and search indexing work. Preserve FTS
   and cold-tier lag reporting.
4. `ft-1grhq.5`: expose the scheduler snapshot in the resource cockpit and
   operator logs with stable reason codes.
5. `ft-1grhq.6`: run storage IO stress and chaos proof, including the
   `ft-lmg3g.4` handoff scenarios above.

## Verification

Contract-only verification for `ft-1grhq.1`:

```bash
git diff --check -- docs/proposals/ft-1grhq-storage-io-scheduler-contract.md
```

Implementation beads must use RCH for Cargo proof. The closeout lane should
include focused commands such as:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-1grhq-storage-io cargo test -p frankenterm-core storage_io_scheduler
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-1grhq-storage-io cargo test -p frankenterm-core ft_lmg3g_storage_io
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-1grhq-storage-io cargo clippy -p frankenterm-core --all-targets -- -D warnings
```

Until `ft-tn6cw.1` is unblocked, failed RCH wrapper proof must be reported
as an infrastructure blocker, not as a scheduler implementation result.
