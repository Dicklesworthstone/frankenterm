# ft-3zr5f — semantic_chunk_embeddings orphan-vector decision

**Status:** Dormant + guard active. Architectural decision deferred until the vector store gets a production caller.
**Bead:** ft-3zr5f (HIGH bug; status converted from BLOCKED to in_progress + closed under this proposal).
**Author:** pane1 (Claude Opus 4.7) · 2026-05-01.

## Problem statement

`semantic_chunk_embeddings` (in
`crates/frankenterm-core/src/search/chunk_vector_store.rs`) stores raw
`pane_id`, `start_segment_id`, `end_segment_id` as INTEGERs but its
DDL has only a composite FK to `semantic_generations(profile_id,
generation_id)` — no FK to `panes(pane_id)` or `output_segments(id)`.

Two theoretical failure modes:

1. **Retention orphans**: `prune_segments_sync` in
   `storage.rs:14157` deletes from `output_segments` based on
   `captured_at`. Cascade cleans `segment_embeddings` (FK in main DB).
   `semantic_chunk_embeddings` rows in the *separate* vector DB still
   carry the now-deleted segment IDs — orphans.
2. **Pane-deletion orphans**: pane tombstone cascades through main-DB
   tables. `semantic_chunk_embeddings` rows keyed on the deleted
   `pane_id` persist indefinitely.

The bead's "obvious fix" — add `FOREIGN KEY(pane_id) REFERENCES
panes(pane_id) ON DELETE CASCADE` — is **not implementable** in the
current architecture. SQLite foreign keys cannot reference a parent
table that lives in a different database file (`ATTACH`-ed or not).
cod_4's audit comment confirmed this with a direct repro:

```bash
sqlite3 ':memory:' "PRAGMA foreign_keys=ON;
                    CREATE TABLE child(x INTEGER REFERENCES parent(id));
                    INSERT INTO child(x) VALUES (1);"
# Error: no such table: main.parent
```

## Current production state

The bug is **dormant** today. Live-code audit (2026-05-01):

```
$ rg -lE "ChunkVectorStore::(open|new)" --include="*.rs" crates/ frankenterm/
crates/frankenterm-core/tests/proptest_chunk_vector_store.rs
crates/frankenterm-core/tests/chunk_vector_store_lifecycle_tests.rs
crates/frankenterm-core/src/search/chunk_vector_store.rs
```

Every `ChunkVectorStore::open`/`new` call is in:

- `crates/frankenterm-core/tests/proptest_chunk_vector_store.rs` (test crate)
- `crates/frankenterm-core/tests/chunk_vector_store_lifecycle_tests.rs` (test crate)
- The same file's inline `#[cfg(test)] mod tests` (lines after 1403)

There is **no production caller** outside test code. `Config` and
`StorageConfig` have no `vector_store_path` / `chunk_store_path`
field. The code is shipped as a future-use library; until something
in the ingest or retention path calls `ChunkVectorStore::open(...)`
with a real path, no orphans accumulate in production.

## Decision

**Defer the architectural decision until a production caller is
needed**, and **add a regression guard now** that fires the moment a
non-test caller is introduced. The guard's failure message points
the future agent at this proposal so the architectural choice is
made *before* the wiring lands, not after.

The three architectural options remain on the shelf:

### Option A — Rehome the vector store into the main storage DB

Move `semantic_chunk_embeddings` and `semantic_generations` from the
standalone vector DB into `storage.rs`'s main DB. FKs work naturally
with `ON DELETE CASCADE` against `panes(pane_id)`.

**Pros**:
- Single source of truth.
- FK durability via SQLite engine.
- Retention cleanup happens automatically on pane/segment deletion.

**Cons**:
- Main DB grows with vector data (potentially large blobs).
- `storage.rs` becomes responsible for the vector schema; another
  layer of cross-cutting concern.
- Migration cost: existing standalone vector DBs (if any) must be
  drained or backfilled.

### Option B — Mirror parent tables into the vector DB

Replicate enough of `panes` and `output_segments` into the vector DB
that local FKs work. Keep the two DBs as separate stores.

**Pros**:
- Vector DB stays separable (e.g. ship to a different host, GC
  independently).
- FK durability via local SQLite.

**Cons**:
- Two sources of truth → drift risk between mirrors.
- Sync cost on every pane/segment write.
- Complex enough that the "mirror logic" itself becomes a
  defect-magnet category — what was a single FK now needs a
  consistency invariant the application enforces.

### Option C — Explicit orphan-cleanup contract (no FK)

Keep tables in separate DBs. Wire `prune_chunks_through_ordinal`
into the retention worker that calls `prune_segments_sync`. Add an
explicit orphan-detection sweep that runs periodically and flags
(or deletes) `semantic_chunk_embeddings` rows whose
`(pane_id, segment_id)` no longer exist in the main DB.

**Pros**:
- Minimal architectural disruption.
- Vector DB stays separable.
- The "orphan cleanup" is observable (logs, metrics) where a FK
  is invisible.

**Cons**:
- No engine-level durability guarantee — relies on the application
  to call the cleanup.
- Window between deletion and cleanup is unbounded unless the
  retention worker runs frequently.
- More code than the other two options.

## Recommendation

When the architectural decision is forced (i.e. when a production
caller is being added), **prefer Option A (rehome)** unless one of
these escape conditions applies:

- **Vector data has grown ≥10% of total DB size** — rehoming
  becomes a footgun for small-instance users; switch to **Option C**
  (explicit cleanup) to keep the main DB lean.
- **A second consumer of the vector data exists** (e.g. a separate
  semantic-search-as-a-service binary) — switch to **Option B**
  (mirror) so the second consumer can read without reaching into
  the main DB.

Option A is the default because:

1. It collapses the cross-DB FK problem to nothing — SQLite handles
   the cascade.
2. The current vector DB has only two tables (`semantic_chunk_embeddings`
   + `semantic_generations`); merging them into the main DB is a
   small, contained migration.
3. The "vector store gets a production caller" event implies a
   request-path consumer (ingest or search), which already runs
   under the main DB connection — no new connection management.
4. ft-d6shp's "monolith vs sub-crate" stance has already accepted
   that `frankenterm-core` is the right home for cross-cutting
   storage; vectors fit that pattern.

## Regression guard (shipped under this proposal)

`crates/frankenterm-core/tests/ft_3zr5f_chunk_vector_store_dormant.rs`
asserts that `ChunkVectorStore::{open, new}` and
`prune_chunks_through_ordinal` have **zero production callers**. The
guard fails CI if any non-test file under `crates/` or `frankenterm/`
introduces such a call without first updating this proposal to
reflect the chosen architectural option (and rewriting the guard's
allowlist).

The error message points at this file:

```
ft-3zr5f guard: ChunkVectorStore introduced into a production path
without resolving the orphan-vector question. Read
docs/proposals/ft-3zr5f-semantic-chunk-orphan-decision.md and
either implement Option A/B/C or update the allowlist with a
rationale comment.
```

## Follow-up bead

Filed as `ft-XXX` (filed in close-out commit) — implement the chosen
architectural option when a production caller is needed. Until that
trigger fires, this bead remains closed and the regression guard
keeps the project honest.

## Why this resolution

Without this decision, ft-3zr5f sat as a HIGH-priority blocked bead
since 2026-04-24. Two unsuccessful claims (jemanuel, cod_4) both
concluded the fix is non-trivial. Leaving it open as "blocked"
indefinitely is a credibility hazard: a HIGH bug that nobody
implements isn't HIGH, it's mistracked.

Closing it as "dormant + guard active" is honest:

- The bug *is* real, but it doesn't fire today.
- The guard catches the moment it could fire.
- The architectural choice is documented so the next agent doesn't
  re-debate it.
- The follow-up bead has a precise trigger condition.

This pattern (decision-by-default + regression-guard) is the same
shape used for ft-i2eni.6 step 2 (audit-vendored-fork.sh
generalization deferred until a second fork lands) and for
ft-h4d9c step 2.
