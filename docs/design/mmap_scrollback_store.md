# Mmap Scrollback Store (ft-8vla)

## Context

`frankenterm-core` persists captured pane output to SQLite (`output_segments`) for durability and search. The `ft-8vla` track adds a file-backed mirror lane that supports fast tail retrieval for large scrollbacks while preserving SQLite as the source of truth.

## Current Status (2026-03-09)

The implementation is now integrated into `storage.rs` behind runtime gates:

- `FT_STORAGE_MMAP_ENABLE=true|1|yes|on` enables the mirror lane.
- `FT_STORAGE_MMAP_DIR` optionally overrides the mirror directory.
- Without explicit `FT_STORAGE_MMAP_DIR`, the lane defaults to `<db_stem>.mmap_scrollback` next to the SQLite database.

## Runtime Architecture

### 1. Write path

- SQLite append (`append_segment_sync`) executes first.
- On successful SQLite append, segment content is mirrored into a per-pane log lane (`MmapScrollbackStore`).
- Mirror-line payloads are single-line JSON envelopes (`MmapSegmentLine`) so multiline segment content is safe in line-oriented files.
- If mirror append fails, mirror writes are disabled and the system continues on SQLite-only mode.

### 2. Read path

- `StorageHandle::get_segments` checks whether the mirror lane is configured.
- If configured, `query_segments_from_mmap` is attempted first.
- Any mirror preparation/read/decode issue falls back to SQLite query semantics.
- If the pane is unknown in the mirror lane, SQLite is used without erroring.

### 3. Fallback guarantees

- SQLite remains authoritative for correctness and recovery.
- Mirror-lane corruption or path failures do not block normal read/write operations.
- Fallback behavior is covered by deterministic tests for both write-path and read-path failures.

## Data Model

Per-pane append log files:

- `${base_dir}/{pane_id}.log` containing one JSON segment envelope per line.

Store API in `mmap_store.rs` includes:

- `append_line(pane_id, line)`
- `tail_lines(pane_id, n)`
- `line_count(pane_id)`
- `pane_storage_mode(pane_id)` for mmap vs sqlite-fallback mode introspection.

## Validation Matrix

### Unit / integration tests

- `mmap_segment_line_round_trip_preserves_multiline_content`
- `get_segments_prefers_mmap_lane_and_falls_back_to_sqlite_on_decode_error`
- `store_falls_back_to_sqlite_when_log_path_is_unwritable`
- `store_falls_back_to_sqlite_when_mmap_tail_offsets_are_invalidated`

### Property tests

- `crates/frankenterm-core/tests/proptest_mmap.rs` covers offset helpers and store invariants.

### Benchmarks

- `crates/frankenterm-core/benches/mmap_scrollback.rs` includes mmap-vs-sqlite append and tail comparisons.

### E2E harness

- `tests/e2e/test_ft_8vla_mmap_scrollback.sh`
- Emits structured JSONL logs (`tests/e2e/logs/ft_8vla_mmap_scrollback_*.jsonl`) with nominal, failure-injection, recovery, and benchmark-compile checks.
- Executes cargo workloads through `rch exec -- ...` and now fails closed if workers are unreachable or `rch` falls back to local execution.
- Uses `cargo check -p frankenterm-core --bench mmap_scrollback --message-format short` for the benchmark compile contract. This matches the repo's other bench guards, stays on `rch`'s remote-only path, and avoids the unstable full remote bench-link path that was failing independently of the mmap logic.
- On macOS, forces `TMPDIR=/tmp` for `rch` invocations to avoid ControlMaster socket path-length failures from masquerading as remote-offload success.

## Open Follow-ons

- Replace current file-read implementation with true mapped read windows once a safe abstraction compatible with workspace lint policy (`unsafe_code = forbid`) is adopted.
- Decide whether to persist a dedicated offset index file (`.idx`) for faster cold-start reconstruction.
- Evaluate compaction/segment-rolling policy for long-lived panes.
