# RFC: Migration Engine M0-M5 Readiness Pattern

## Status
Proposed

Implementation truth: the current `MigrationEngine` owns import verification
and a durable M5 readiness marker only. It does not own a process-wide backend
selector and cannot atomically persist or switch one. Selector activation is a
separate, currently unimplemented authority; M5 success must not be reported as
a completed migration.

## Problem
Migrating a live terminal recorder from one storage backend to another (e.g., AppendLog → SQLite) requires careful staging to avoid data loss, ensure rollback capability, and establish readiness before an external selector authority performs cutover.

## Solution
A six-stage migration pipeline where each stage has clear entry criteria, invariants, and exit evidence:

### Stages
| Stage | Name | Description |
|-------|------|-------------|
| M0 | Preflight | Check source health and capture a manifest from the supplied reader |
| M1 | Export | Read the source event stream and compute count/ordinal digest evidence |
| M2 | Import | Transfer events in content-bound batches and validate each append receipt |
| M3 | Checkpoint sync | Copy selected consumer checkpoints with monotonic commit checks |
| M4 | Reserved | Projection reconciliation is handled by external reindex hooks, not this engine |
| M5 | Readiness | Validate manifest parity and target health, then fsync a readiness marker; do not activate a selector |

### Key Properties
- **Bounded retry identity**: M2 batch IDs include their ordinal range and a SHA-256 content digest. Rusqlite retains receipts across reopen; AppendLog's general receipt cache is process-local.
- **Not yet a resumable orchestrator**: the engine does not durably persist a stage cursor or resume index, and M0 does not acquire a true quiesced source snapshot.
- **Source non-mutation**: current M0/M1 read the source and no stage switches the live selector. A reported source path is only an unverified coordination hint; the eventual activation/rollback authority remains external.
- **Observable**: Each stage emits structured log events with stage, progress, and duration.

### Current authority boundary

`MigrationEngine::m5_mark_ready` validates M0/M1/M2 count parity, M1/M2
digest parity, the checked marker ordinal, and target health before marker I/O.
It rechecks health after the fsync append and returns a typed error that retains
the marker offset if the target degrades. It also rejects reserved backend
identities before any storage I/O.

The current `RecorderStorage` target surface does not provide a read cursor, so
M5 can verify the manifest produced by M2 but cannot independently recompute a
digest by reading the target back. A future activation authority must add that
readback proof and atomically persist/switch the backend selector. Until then,
no M5 result, marker, or log constitutes activation or migration completion.

### MigrationConfig
```rust
struct MigrationConfig {
    export_batch_size: usize,
    import_batch_size: usize,
    consumer_id: String,
}
```

## Testing Guidance
- Test each stage independently with fixture data
- Verify content-bound M2 retries against both backend retention contracts
- Property test: preflight, export, and import counts/digests agree before M5 marker I/O
- Prove degraded targets, reserved selectors, mismatched manifests, and ordinal overflow perform zero marker I/O
- Prove post-marker degradation returns an error containing the durable marker offset
- Keep the missing durable stage cursor, source-instance identity, quiesced snapshot, and target-readback proof explicit until those authorities exist
