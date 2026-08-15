# Storage Backend Trait — FrankenSQLite Migration Substrate

**Bead:** `wa-2l27x.8` (FrankenSQLite migration plan).
**Module:** [`crates/frankenterm-core/src/storage_backend_trait.rs`](../../crates/frankenterm-core/src/storage_backend_trait.rs).
**Parent epic:** `wa-2l27x` (Crash-Resilient Session Persistence via FrankenSQLite).

## Why this exists

`wa-2l27x.8` was originally filed with this release prerequisite:

> Status: DEFERRED until frankensqlite Phase 5+ ships

That upstream prerequisite is now satisfied by the published `fsqlite`
facade. The real backend is still deferred because FrankenTerm must first
converge its dependency graph on one compatible Asupersync runtime and finish
exclusive transaction ownership. The six migration tasks remain:

1. Add `fsqlite` as a workspace dependency — needs the one-runtime cohort.
2. **Create storage backend trait to abstract rusqlite vs frankensqlite — shippable today.**
3. Implement the `fsqlite` backend behind a feature flag — needs (1) and the
   transaction-ownership prerequisite.
4. Benchmark single-writer / concurrent-writer / checkpoint latency — needs (3).
5. Migration tool: convert existing rusqlite `.db` to frankensqlite — needs (3).
6. Gradual rollout — needs (3) + (5).

Task #2 — the trait — is useful independently of backend integration:

- The trait is the boundary the eventual `fsqlite` integration rides on.
- It also improves storage.rs's testability via a mockable backend.

Either way, the work pays for itself.

## What the substrate ships

[`crates/frankenterm-core/src/storage_backend_trait.rs`](../../crates/frankenterm-core/src/storage_backend_trait.rs), with an extensive inline test suite:

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
- **`RusqliteBackend`** — the wired persistent implementation and factory over
  a real `rusqlite::Connection`; the remaining extraction work is threading
  more of `storage.rs` through this boundary.
- **`MockBackend`** — in-memory mock for testing. Records executed statements + tracks transaction state + answers `user_version` queries. Useful in storage.rs unit tests today, before any frankensqlite migration.

## What the substrate intentionally does NOT ship

- **Refactor of storage.rs.** storage.rs is large and uses
  `rusqlite::Connection` directly throughout. Threading the trait through every
  call site is a multi-week refactor filed as **`wa-2l27x.8.cont.extract`**.
- **Real `FrankenSQLiteBackend` impl.** Blocked on one-runtime dependency
  convergence and exclusive transaction ownership. Filed as
  **`wa-2l27x.8.cont.frankensqlite`**.
- **Side-by-side benchmarks.** Per the bead's task #4, requires both impls. Filed as **`wa-2l27x.8.cont.benchmarks`**.
- **rusqlite → frankensqlite migration tool.** Per the bead's task #5. Filed as **`wa-2l27x.8.cont.migration_tool`**.

## Migration roadmap

```
wa-2l27x.8 (closed parent migration plan)
├── ✓ trait substrate (this bead — shipped)
├── ◐ wa-2l27x.8.cont.extract       — RusqliteBackend shipped; continue storage.rs adoption
├── ○ wa-2l27x.8.cont.frankensqlite — fsqlite backend impl (blocked: runtime cohort + transaction ownership)
├── ○ wa-2l27x.8.cont.benchmarks    — bench rusqlite vs frankensqlite (blocked: cont.frankensqlite)
└── ○ wa-2l27x.8.cont.migration_tool — .db converter (blocked: cont.frankensqlite)
```

The substrate and concrete `RusqliteBackend` have begun **cont.extract**.
Rusqlite remains the only wired persistent implementation; the trait is the
boundary that the eventual `fsqlite` implementation will use.

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

Representative tests in `storage_backend_trait::tests` include:

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
- `crates/frankenterm-core/src/storage.rs` — the current rusqlite-backed
  implementation. `cont.extract` refactors this through the trait.
- `crates/frankenterm-core/src/storage_targets.rs` — adjacent storage-layer module; not affected by this substrate.
- frankensqlite project: github.com/Dicklesworthstone/frankensqlite — the
  release prerequisite is satisfied; runtime-cohort and transaction-ownership
  work still gate `cont.frankensqlite`.
- ft-2okh0.5 (closed — crash-safe scrollback substrate, sibling crash-resilience work).
