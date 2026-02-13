# Mmap Scrollback Store (wa-8vla)

## Context

`frankenterm-core` stores captured output in SQLite (`output_segments`). This is durable and searchable, but very large scrollback reconstruction paths (`tail` reads, restore/replay contexts) still create avoidable heap pressure when large slices are materialized eagerly.

The `wa-8vla` objective is an mmap-backed, append-oriented scrollback content lane where read paths can rely on OS page cache rather than large user-space allocations.

## Scope for This Slice

This first slice establishes:

- Proposed storage contract and on-disk shape
- Rust API scaffold for a pane-scoped append/read store
- Proptest and benchmark scaffolding for offset/index invariants

This slice does **not** yet wire the store into `storage.rs` or `robot get-text` production paths, to avoid collisions with active concurrent reservations.

## Design Direction

### 1. Data plane

Per-pane append log files:

- `${base_dir}/{pane_id}.log` (raw UTF-8 bytes + `\n` delimiters)
- `${base_dir}/{pane_id}.idx` (line start offsets, LE u64)

Current scaffold keeps index in memory and appends payload bytes to `.log`; mmap integration will swap read path from `read_to_end` to page-window reads over mapped regions.

### 2. API surface

`MmapScrollbackStore` should support:

- `append_line(pane_id, line)`
- `tail_lines(pane_id, n)`
- `line_count(pane_id)`
- `flush(pane_id)` / checkpoint hooks

### 3. Invariants

- Offsets are monotonic non-decreasing.
- Every index entry points to a valid byte position within log file bounds.
- Tail reads never cross negative index boundaries.
- Page alignment helper is deterministic and idempotent.

### 4. Benchmarks (planned)

- Offset-building throughput from variable line lengths.
- Tail-window extraction from offset arrays.
- Page-alignment helper hot-path overhead.

## Integration Plan (Follow-on)

1. Add `memmap2` dependency and implement mapped read windows.
2. Add durability metadata (checkpoint offset, file generation).
3. Integrate behind feature/config gate in `storage` read path.
4. Add fallback path to SQLite-only reads on mapping/open failures.
5. Add comparative Criterion runs (`mmap` vs `sqlite`) on representative corpora.

## Open Questions

- Keep `.idx` fully memory-mirrored, or mmap index file too?
- Best compaction strategy (segment rollovers vs periodic rewrite)?
- Should search remain SQLite-only while content retrieval uses mmap, or should we co-store text shards for hybrid retrieval?
