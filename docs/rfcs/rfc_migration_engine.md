# RFC: Migration Engine M0-M5 Stage Pattern

## Status
Proposed

## Problem
Migrating a live terminal recorder from one storage backend to another (e.g., AppendLog → SQLite) requires careful staging to avoid data loss, ensure rollback capability, and maintain system availability during cutover.

## Solution
A six-stage migration pipeline where each stage has clear entry criteria, invariants, and exit evidence:

### Stages
| Stage | Name | Description |
|-------|------|-------------|
| M0 | Inventory | Enumerate source events, compute digests |
| M1 | Schema Prep | Create target schema, validate compatibility |
| M2 | Bulk Copy | Transfer events in batches with progress tracking |
| M3 | Verify | Compare source/target digests, count parity |
| M4 | Catchup | Replay events written during M2-M3 window |
| M5 | Cutover | Activate target as primary, keep source as fallback |

### Key Properties
- **Resumable**: Each stage checkpoints progress. Crash at M2.batch[47] resumes from batch 47.
- **Idempotent**: Re-running any stage with same input produces same output.
- **Rollback-safe**: M0-M4 are read-only on source. M5 cutover is the only point of no return, and even then source data is preserved.
- **Observable**: Each stage emits structured log events with stage, progress, and duration.

### MigrationConfig
```rust
struct MigrationConfig {
    source_path: PathBuf,
    target_path: PathBuf,
    batch_size: usize,
    checkpoint_interval: usize,
    dry_run: bool,
}
```

## Testing Guidance
- Test each stage independently with fixture data
- Verify resumability: interrupt at M2, resume, verify completion
- Property test: source event count == target event count after M3
- Test dry_run mode produces no side effects on target
