# Storage and Index Tuning

ft uses SQLite in WAL (Write-Ahead Logging) mode with a single-writer,
multiple-reader architecture. Full-text search is powered by FTS5. This
document covers how to monitor, tune, and maintain both.

## Architecture Overview

```
Write path:  caller → StorageHandle → mpsc queue → writer thread → SQLite (WAL)
Read path:   caller → StorageHandle → new read connection → SQLite (WAL)
Search path: caller → FTS5 query → output_segments_fts virtual table
```

- **Single writer:** All writes go through a bounded mpsc channel to a
  dedicated thread. This serializes writes and avoids SQLite lock
  contention.
- **Multiple readers:** Read operations open their own connections and
  run concurrently with the writer thanks to WAL mode.
- **Write queue:** Bounded at 1,024 commands by default. When the queue
  fills, callers experience backpressure.

### SQLite Configuration

ft sets these PRAGMAs at connection time:

| PRAGMA | Value | Purpose |
|--------|-------|---------|
| `journal_mode` | WAL | Concurrent reads during writes |
| `synchronous` | NORMAL | Balance between safety and speed |
| `foreign_keys` | ON | Enforce referential integrity |

WAL mode allows readers to proceed without blocking the writer, and vice
versa. The trade-off is a WAL file that grows until checkpointed.

## Full-Text Search (FTS5)

### What is indexed

The `output_segments_fts` virtual table indexes the `content` column of
`output_segments`. Tokenization uses Porter stemming with Unicode
normalization (`porter unicode61`), so searches match word stems across
languages.

### How indexing works

FTS is updated **in the same transaction** as the source data via SQLite
triggers:

- `INSERT` on `output_segments` → corresponding FTS insert
- `UPDATE` on `output_segments` → FTS delete + insert
- `DELETE` on `output_segments` → FTS delete

This means FTS is always consistent with the source table under normal
operation. There is no indexing lag unless the FTS index is corrupt or
was rebuilt incompletely.

### FTS query syntax

ft supports FTS5 query syntax:

```bash
# Simple term
ft search "error"

# Phrase
ft search '"connection refused"'

# Prefix
ft search "timeout*"

# Boolean
ft search "error AND NOT warning"

# Combined
ft search '"api key" OR "access token"'
```

Invalid syntax is caught by the query linter before execution. Use
`ft search fts verify` to check index health if queries return
unexpected results.

## Monitoring Storage Health

### Quick health check

```bash
ft db check
```

Runs five checks and reports pass/fail:

1. **SQLite integrity** — `PRAGMA quick_check` (fast, not full)
2. **Schema version** — validates `PRAGMA user_version` matches expected
3. **Foreign key consistency** — `PRAGMA foreign_key_check`
4. **FTS index integrity** — FTS5 `integrity-check` command
5. **WAL checkpoint status** — checks WAL frame count against threshold

Exit codes: 0 = OK, 1 = errors found, 2 = warnings only.

For machine-readable output:

```bash
ft db check -f json
```

### FTS index health

```bash
ft search fts verify
```

Reports:
- Total segments and FTS rows (should match)
- Number of inconsistent panes (should be zero)
- Per-pane details in verbose mode

If `inconsistent_panes > 0`, the FTS index is out of sync with the
source data. Run a rebuild (see below).

### Doctor diagnostics

```bash
ft doctor
```

Includes storage checks alongside environment, config, and runtime
health. The `--circuits` flag adds circuit breaker status for WezTerm
CLI calls.

## Maintenance Commands

### Checkpoint and optimize (routine)

Under normal operation, SQLite checkpoints the WAL automatically. ft
also runs periodic maintenance:

- **Passive checkpoint** — non-blocking, runs during the normal write
  cycle
- **Full checkpoint** — triggered when WAL exceeds 10,000 frames,
  truncates WAL to reclaim space
- **Query planner update** — `PRAGMA optimize` refreshes statistics for
  better query plans

This happens automatically. Manual intervention is rarely needed.

### FTS rebuild (when index is unhealthy)

```bash
ft search fts rebuild
```

Drops the FTS index and reindexes all segments from scratch. This runs
in batches (100 segments / 1 MiB per batch) with progress tracking per
pane. The operation is safe to interrupt — progress is committed after
each batch, so a restart resumes from the last committed position.

Output includes:
- Panes processed
- Segments indexed
- Duration in milliseconds
- Any non-fatal warnings

### Database repair (when checks fail)

```bash
ft db repair              # interactive — prompts for confirmation
ft db repair --dry-run    # preview repairs without executing
ft db repair --yes        # skip confirmation
ft db repair -f json      # machine-readable output
```

Repair performs:
1. **Backup** — creates `db.bak.{timestamp}` unless `--no-backup`
2. **FTS rebuild** — reindexes all full-text data
3. **WAL checkpoint** — forces truncation checkpoint
4. **VACUUM** — rewrites the database file to reclaim space and defragment

Each step reports success or failure. Exit code 1 on any failure.

### VACUUM (space reclamation)

VACUUM is the most expensive operation — it rewrites the entire database
file. Use it only when:
- The database has grown significantly from deletions (retention cleanup)
- You need to reduce the on-disk footprint
- `ft db repair` includes it as part of a full repair

VACUUM blocks all access while running. For large databases, schedule it
during maintenance windows.

### Schema migrations

```bash
ft db migrate              # run pending migrations
ft db migrate --status     # check current version and pending migrations
ft db migrate --to 5       # migrate to a specific version
ft db migrate --dry-run    # preview without applying
```

Migrations are versioned and support rollback. The schema version is
tracked in `PRAGMA user_version`.

## Tuning for Performance

### Write throughput

The write queue bounds how many operations can be pending before
backpressure kicks in. The default (1,024) handles most workloads. If
you observe frequent backpressure under heavy capture:

- Increase the write queue size (requires code change in `StorageConfig`)
- Reduce capture frequency via tailer configuration
- Check that the storage device has adequate I/O bandwidth

### Search performance

FTS5 queries are fast for typical workloads. If search feels slow:

1. **Check index health:** `ft search fts verify`
2. **Rebuild if needed:** `ft search fts rebuild`
3. **Check WAL size:** A large WAL can slow reads. Force a checkpoint
   with `ft db repair` or wait for the automatic 10,000-frame trigger.
4. **Check database size:** Very large databases benefit from retention
   cleanup to remove old segments.

### WAL growth

The WAL file grows with write activity and shrinks when checkpointed.
Under normal operation:

- Passive checkpoints run automatically during writes
- Full checkpoint + truncation triggers at 10,000 frames
- After repair or explicit checkpoint, WAL shrinks to near zero

If the WAL file is unexpectedly large, check for long-running read
transactions that prevent checkpointing.

### Retention cleanup

Remove old segments to control database growth:

```rust
// Programmatic: delete segments older than a threshold
storage.retention_cleanup(before_ts_epoch_ms).await?;
```

The retention system is documented separately in the retention tiers
policy. Cleanup events are logged to the `maintenance_log` table for
auditing.

## Collecting Performance Evidence

When filing a bug report about storage performance:

1. Run `ft db check -f json` and include the output
2. Run `ft search fts verify` and include the output
3. Note the database file size (`ls -lh` on the `.db` file)
4. Note the WAL file size (`ls -lh` on the `.db-wal` file)
5. Generate a diagnostic bundle: `ft diag bundle`

The diagnostic bundle captures database metadata, row counts, WAL
status, and recent events automatically.

## Internal Tables

For reference, these are the tables relevant to storage tuning:

| Table | Purpose |
|-------|---------|
| `panes` | Active and observed pane metadata |
| `output_segments` | Captured pane output (content, timestamps, sequence) |
| `output_segments_fts` | FTS5 virtual table indexing `output_segments.content` |
| `fts_index_state` | Singleton: FTS version, last rebuild timestamp |
| `fts_pane_progress` | Per-pane FTS sync progress (last indexed sequence) |
| `events` | Detected events (matches, patterns, annotations) |
| `maintenance_log` | Audit log for startup, shutdown, vacuum, cleanup |
| `reservations` | Pane reservation tracking |

## Troubleshooting

### "Database locked" errors

The single-writer architecture should prevent lock contention. If you
see `FT-2001` (database locked):

1. Check for external tools accessing the same database file
2. Ensure only one ft instance is running per database
3. Use `ft robot why FT-2001` for recovery guidance

### FTS returns stale results

Run `ft search fts verify`. If inconsistent panes are reported, rebuild:

```bash
ft search fts rebuild
```

### Database grows unexpectedly

Check the maintenance log for retention cleanup events. If cleanup is
not running, verify retention configuration. Run `ft db repair` to
VACUUM and reclaim space from deleted rows.

### WAL file is very large

A large WAL indicates the checkpoint is not keeping up. Possible causes:
- Long-running read transactions blocking checkpoint
- Very high write throughput exceeding checkpoint capacity
- Checkpoint disabled or failing silently

Force a checkpoint: `ft db repair --yes`
