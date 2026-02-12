# Recorder Storage Abstraction + Backend-Gated Configuration Contract (`ft-oegrb.3.1`)

Date: 2026-02-12  
Status: Approved design contract for implementation  
Owners: `OrangeBarn` + storage track implementers

## Why this exists
The flight recorder already has capture-facing contracts (`RecorderEvent`, ingress tap, egress tap), but it still needs a backend-agnostic durability boundary so we can:
1. Ship a low-risk append-log backend first.
2. Integrate a FrankenSQLite-backed backend without rewriting capture/index/query layers.
3. Keep failure handling explicit and deterministic under swarm-scale load.

This document is the canonical contract for `ft-oegrb.3.1` and is intentionally detailed so downstream beads (`ft-oegrb.3.2`, `.3.3`, `.3.4`, `.3.6`) can implement without re-litigating interface shape.

Bead ID note:
- `ft-oegrb.*` is the active tracker namespace in this repo.
- `wa-oegrb.*` references in older threads/docs are legacy aliases for the same flight-recorder graph nodes.

## Inputs and cross-references
- Event contract: `docs/flight-recorder/recorder-event-schema.md`
- ADR: `docs/flight-recorder/adr-0001-flight-recorder-architecture.md`
- FrankenSQLite adaptation findings: `docs/flight-recorder/frankensqlite-append-log-dossier.md`
- Cross-project synthesis: `docs/flight-recorder/cross-project-extraction-matrix.md`
- Current capture surfaces: `crates/frankenterm-core/src/recording.rs`

## Scope and non-goals
Scope:
- Stable `RecorderStorage` interface and data contracts.
- Backend selection and fallback configuration model.
- Failure taxonomy and retry semantics.
- Durability and checkpoint semantics for append-only ingestion.

Non-goals:
- Full implementation of append writer hot path (that is `ft-oegrb.3.2`, legacy `wa-oegrb.3.2`).
- FrankenSQLite adapter implementation (that is `ft-oegrb.3.3`, legacy `wa-oegrb.3.3`).
- Indexing/query interfaces (tracks `ft-oegrb.4` and `ft-oegrb.5`, legacy `wa-oegrb.4`/`wa-oegrb.5`).

## Placement in architecture
Capture taps (`IngressTap` / `EgressTap`) remain producer-only and backend-agnostic.  
`RecorderStorage` is the only durability contract that capture is allowed to call.

Data flow:
1. Capture code emits validated `RecorderEvent` values.
2. Recorder ingest batches events and submits append requests through `RecorderStorage`.
3. Storage returns canonical offsets/checkpoints.
4. Indexers consume canonical offsets asynchronously.

## Contract: `RecorderStorage`

### Trait shape (normative)
```rust
pub trait RecorderStorage: Send + Sync {
    fn backend_kind(&self) -> RecorderBackendKind;

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> Result<AppendResponse, RecorderStorageError>;

    async fn flush(&self, mode: FlushMode) -> Result<FlushStats, RecorderStorageError>;

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> Result<Option<RecorderCheckpoint>, RecorderStorageError>;

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> Result<CheckpointCommitOutcome, RecorderStorageError>;

    async fn health(&self) -> RecorderStorageHealth;

    async fn lag_metrics(&self) -> Result<RecorderStorageLag, RecorderStorageError>;
}
```

### Backend kinds
```rust
pub enum RecorderBackendKind {
    AppendLog,
    FrankenSqlite,
}
```

### Append request/response contracts
```rust
pub struct AppendRequest {
    pub batch_id: String,
    pub events: Vec<RecorderEvent>,
    pub required_durability: DurabilityLevel,
    pub producer_ts_ms: u64,
}

pub enum DurabilityLevel {
    Enqueued,
    Appended,
    Fsync,
}

pub struct AppendResponse {
    pub backend: RecorderBackendKind,
    pub accepted_count: usize,
    pub first_offset: RecorderOffset,
    pub last_offset: RecorderOffset,
    pub committed_durability: DurabilityLevel,
    pub committed_at_ms: u64,
}

pub struct RecorderOffset {
    pub segment_id: u64,
    pub byte_offset: u64,
    pub ordinal: u64,
}
```

Norms:
- `batch_id` must be idempotency-safe. Duplicate `batch_id` append attempts must return the original committed result (or a deterministic idempotency error).
- `events` must preserve producer order. Storage may reject out-of-contract ordering but may not silently reorder.
- `RecorderOffset.ordinal` is globally monotonic for the chosen backend instance and is the canonical replay/index cursor.

### Checkpoint contract
Checkpoint consumers are named durable readers (for example, lexical indexer, semantic indexer, replay tool).

```rust
pub struct CheckpointConsumerId(pub String);

pub struct RecorderCheckpoint {
    pub consumer: CheckpointConsumerId,
    pub upto_offset: RecorderOffset,
    pub schema_version: String,
    pub committed_at_ms: u64,
}

pub enum CheckpointCommitOutcome {
    Advanced,
    NoopAlreadyAdvanced,
    RejectedOutOfOrder,
}
```

Norms:
- Checkpoint commits are monotonic per consumer.
- Out-of-order regressions are rejected, never silently accepted.
- Checkpoints are independent across consumers.

## Configuration model (backend-gated runtime selection)

### Proposed config shape
```toml
[recorder]
enabled = false

[recorder.storage]
backend = "append_log"           # append_log | frankensqlite
fallback_backend = "append_log"  # deterministic fallback target
startup_policy = "fail_closed"   # fail_closed | fallback
runtime_error_policy = "degrade" # degrade | fallback | stop_capture

# admission and batching
queue_capacity = 10000
max_batch_events = 256
max_batch_bytes = 262144
max_batch_delay_ms = 8

# durability defaults
default_durability = "appended"  # enqueued | appended | fsync
flush_interval_ms = 250

# backpressure
overflow_policy = "drop_newest_with_gap" # reject | drop_newest_with_gap | block_with_timeout
overflow_timeout_ms = 50

[recorder.storage.append_log]
root_dir = ".ft/recorder-log"
segment_target_bytes = 134217728
sync_on_rotate = true

[recorder.storage.frankensqlite]
db_path = ".ft/recorder-frankensqlite.db"
writer_shards = 4
busy_timeout_ms = 75
wal_autocheckpoint_pages = 4000
recovery_mode = "checksum_then_repair"   # checksum_then_repair | checksum_only
```

### Selection and fallback rules (normative)
1. Startup:
   - If configured backend initializes cleanly, use it.
   - If startup fails and `startup_policy=fallback`, initialize `fallback_backend`.
   - If startup fails and `startup_policy=fail_closed`, recorder capture is disabled and surfaced as unhealthy.
2. Runtime failure:
   - `degrade`: remain on current backend but return explicit degraded errors/status.
   - `fallback`: perform one-way failover to `fallback_backend` and emit lifecycle marker.
   - `stop_capture`: disable recorder writes and emit lifecycle marker.
3. Fallback loops are forbidden:
   - one failover attempt per process lifetime unless explicitly reset by operator action.

## Failure taxonomy and retry contract

### Error classes
```rust
pub enum RecorderStorageErrorClass {
    Retryable,
    Overload,
    TerminalConfig,
    TerminalData,
    Corruption,
    DependencyUnavailable,
}
```

Suggested concrete errors:
- `QueueFull` (`Overload`)
- `BusyTimeout` (`Retryable` or `Overload`, backend-specific)
- `IoTransient` (`Retryable`)
- `BackendUnavailable` (`DependencyUnavailable`)
- `InvalidSchemaVersion` (`TerminalData`)
- `SequenceInvariantViolation` (`TerminalData`)
- `CheckpointRegression` (`TerminalData`)
- `CorruptionDetected` (`Corruption`)
- `MisconfiguredBackend` (`TerminalConfig`)

### Retry rules
- Storage must not perform unbounded hidden retries.
- Caller retries only when `error.class` is `Retryable` or configured `Overload` behavior permits it.
- Retry budget defaults:
  - max attempts: 3
  - initial backoff: 5ms
  - exponential factor: 2
  - max backoff: 100ms
- After retry budget exhaustion:
  - emit explicit overflow/degradation marker
  - apply configured `runtime_error_policy`

### Why these boundaries
This keeps hot-path behavior predictable under burst load and matches the FrankenSQLite-derived principle of bounded admission with explicit overload signals rather than hidden queue growth.

## Durability semantics
- `Enqueued`: accepted into bounded in-memory queue; not durable on crash.
- `Appended`: bytes appended to backend write-ahead surface; crash-safe up to backend validity rules.
- `Fsync`: append persisted to durable storage according to OS fsync semantics.

Norms:
- API caller chooses required durability per append batch or uses `default_durability`.
- Backend may return stronger durability than requested, never weaker.
- Returned `committed_durability` is authoritative for SLO/accounting.

## Health and lag metrics contract
`health()` and `lag_metrics()` must support SLO-driven operations and diagnostics.

Required metrics:
- active backend kind
- degraded mode flag + reason
- queue depth and queue capacity
- append success/error counters by class
- p50/p95 append latency
- bytes/sec and events/sec append rates
- latest committed offset
- per-consumer checkpoint lag (offset and time)

These metrics feed `ft-oegrb.3.6` (legacy `wa-oegrb.3.6`) and rollout/operations tracks.

## Contract test requirements (blocking for backend implementations)
Every backend implementing `RecorderStorage` must pass a shared contract suite:
1. Append idempotency with duplicate `batch_id`.
2. Monotonic offset assignment.
3. Ordering preservation under concurrent producers.
4. Checkpoint monotonic commit behavior.
5. Recovery from torn-tail/partial write returns valid prefix.
6. Error classification stability (`Retryable` vs terminal classes).
7. Durability-level guarantee checks.

## ADR note: why this interface is shaped this way
This contract intentionally optimizes for append-heavy ingestion and independent projection pipelines:
- Append/checkpoint are the only cross-component primitives needed for canonical-log + derived-index architecture.
- Idempotent batches + monotonic offsets make replay/index resume deterministic.
- Explicit error classes and bounded retry prevent hidden latency cliffs.
- Backend-gated configuration allows incremental rollout from low-risk backend to FrankenSQLite-backed experiments.

## Downstream implementation handoff
- `ft-oegrb.3.2` (legacy alias: `wa-oegrb.3.2`): implement `AppendLogRecorderStorage` against this contract.
- `ft-oegrb.3.3` (legacy alias: `wa-oegrb.3.3`): implement `FrankenSqliteRecorderStorage` with same contract tests.
- `ft-oegrb.3.4` (legacy alias: `wa-oegrb.3.4`): implement checkpoint/replay machinery using `RecorderOffset` + checkpoint API.
- `ft-oegrb.3.6` (legacy alias: `wa-oegrb.3.6`): implement telemetry surfaces using `health()`/`lag_metrics()` outputs.
