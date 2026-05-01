# Storage Backend Trait — FrankenSQLite Migration Substrate

**Bead:** `wa-2l27x.8` (FrankenSQLite migration plan).
**Module:** [`crates/frankenterm-core/src/storage_backend_trait.rs`](../../crates/frankenterm-core/src/storage_backend_trait.rs).
**Parent epic:** `wa-2l27x` (Crash-Resilient Session Persistence via FrankenSQLite).

## Why this exists

`wa-2l27x.8`'s body says:

> Status: DEFERRED until frankensqlite Phase 5+ ships

That's true for **5 of the 6 migration tasks** the bead names:

1. Add frankensqlite as workspace dependency — needs frankensqlite to ship.
2. **Create storage backend trait to abstract rusqlite vs frankensqlite — shippable today.**
3. Implement frankensqlite backend behind feature flag — needs (1).
4. Benchmark single-writer / concurrent-writer / checkpoint latency — needs (3).
5. Migration tool: convert existing rusqlite `.db` to frankensqlite — needs (3).
6. Gradual rollout — needs (3) + (5).

Task #2 — the trait — is shippable **regardless of frankensqlite's eventual fate**:

- If frankensqlite ships → the trait is the boundary the swap rides on (the bead's stated goal).
- If frankensqlite never ships → the trait improves storage.rs's testability via a mockable backend (a real win independent of any migration).

Either way, the work pays for itself.

## What the substrate ships

[`crates/frankenterm-core/src/storage_backend_trait.rs`](../../crates/frankenterm-core/src/storage_backend_trait.rs) (~370 lines, 8 unit tests):

- **`StorageBackend` trait** — names the operations storage.rs performs conceptually:
  - `execute(sql) -> Result<rows_affected, BackendError>` for DDL + DML.
  - `execute_batch(sql)` for migration scripts.
  - `query_scalar(sql)` for single-value reads.
  - `begin_transaction() -> TransactionGuard<'_>` with RAII commit/rollback.
  - `user_version()` / `set_user_version(v)` for the schema-migration runner.
  - `backend_name()` for diagnostics.
- **`StorageBackendFactory`** — separate trait carrying the `open()` constructor, kept off `StorageBackend` so the latter stays object-safe.
- **`OpenConfig`** — common open-time knobs (read-only, WAL, page size hint).
- **`BackendError`** — common error surface (Connect / Query / TxPoisoned / Schema / Other).
- **`TransactionGuard`** — RAII transaction wrapper. Rolls back on `Drop` by default; explicit `commit()` required for durability.
- **`MockBackend`** — in-memory mock for testing. Records executed statements + tracks transaction state + answers `user_version` queries. Useful in storage.rs unit tests today, before any frankensqlite migration.

## What the substrate intentionally does NOT ship

- **Refactor of storage.rs.** storage.rs is ~26K lines and uses `rusqlite::Connection` directly throughout. Threading the trait through every call site is a multi-week refactor filed as **`wa-2l27x.8.cont.extract`**.
- **Real `RusqliteBackend` impl.** Out of scope for the substrate; the trait shape is the contract, the impl drops in under cont.extract.
- **Real `FrankenSQLiteBackend` impl.** Externally blocked on frankensqlite Phase 5+ shipping. Filed as **`wa-2l27x.8.cont.frankensqlite`**.
- **Side-by-side benchmarks.** Per the bead's task #4, requires both impls. Filed as **`wa-2l27x.8.cont.benchmarks`**.
- **rusqlite → frankensqlite migration tool.** Per the bead's task #5. Filed as **`wa-2l27x.8.cont.migration_tool`**.

## Migration roadmap

```
wa-2l27x.8 (parent — DEFERRED status, but task #2 shippable today)
├── ✓ trait substrate (this bead — shipped)
├── ○ wa-2l27x.8.cont.extract       — refactor storage.rs through the trait
├── ○ wa-2l27x.8.cont.frankensqlite — frankensqlite backend impl (blocked: Phase 5+)
├── ○ wa-2l27x.8.cont.benchmarks    — bench rusqlite vs frankensqlite (blocked: cont.frankensqlite)
└── ○ wa-2l27x.8.cont.migration_tool — .db converter (blocked: cont.frankensqlite)
```

The substrate unblocks **cont.extract**: the refactor can begin
today using rusqlite as the only impl, with the trait serving as
the boundary that frankensqlite's eventual impl drops into.

## Why the trait shape is intentionally minimal

The trait below names operations conceptually, not exhaustively.
Storage.rs uses rusqlite's full surface (prepared statements,
parameter binding, row mapping, FTS5 syntax, custom SQL functions,
PRAGMA introspection). A "perfect" trait that captures all of
that would be a parallel sql-abstraction crate — sqlx-compete.
Out of scope.

The substrate trait covers the **conceptual** surface storage.rs
needs:

- Execute statements (DDL + DML) — covered by `execute` /
  `execute_batch`.
- Read scalar values — covered by `query_scalar` (the
  full row-mapper extension lands under cont.extract when
  storage.rs starts being threaded through).
- Begin / commit / rollback transactions — covered by
  `TransactionGuard`.
- Schema versioning — covered by `user_version` /
  `set_user_version`.

cont.extract will extend the trait as it threads storage.rs
through — adding `prepare(sql) -> Statement`, `Row` accessors,
parameter binding traits, etc. The substrate is the *floor*, not
the ceiling.

## Tests

8 unit tests in `storage_backend_trait::tests`:

| Test | Invariant |
| ---- | --------- |
| `trait_is_dyn_safe` | `Box<dyn StorageBackend>` compiles + dispatches. |
| `mock_records_executed_statements` | The mock log captures every `execute` call in order. |
| `execute_batch_splits_on_semicolons` | Migration scripts decompose correctly. |
| `transaction_commit_records_committed_state` | `commit()` issues `COMMIT` + flips `last_tx_committed`. |
| `transaction_drop_rolls_back_when_not_committed` | RAII guard rolls back on Drop without `commit()`. |
| `user_version_round_trips` | Schema-version probe + setter agree. |
| `open_config_defaults_to_wal_mode_writable` | Defaults match the bead's stated migration target. |
| `backend_error_renders_each_variant` | `Display` impl renders every variant with the `storage backend` prefix. |

## Cross-references

- `wa-2l27x` (parent epic — Crash-Resilient Session Persistence).
- `wa-2l27x.7` (closed — E2E test suite, the prerequisite that the bead's `Depends on` cited).
- `crates/frankenterm-core/src/storage.rs` — the ~26K-line current rusqlite-backed implementation. cont.extract refactors this through the trait.
- `crates/frankenterm-core/src/storage_targets.rs` — adjacent storage-layer module; not affected by this substrate.
- frankensqlite project: github.com/Dicklesworthstone/frankensqlite — Phase 5+ is the external precondition for cont.frankensqlite.
- ft-2okh0.5 (closed — crash-safe scrollback substrate, sibling crash-resilience work).
