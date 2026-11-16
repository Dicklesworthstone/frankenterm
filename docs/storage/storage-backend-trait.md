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
  - `with_transaction(f)` for exclusive, non-interleavable transaction execution.
  - `user_version()` / `set_user_version(v)` for the schema-migration runner.
  - `backend_name()` for diagnostics.
- **`StorageBackendFactory`** — separate trait carrying the `open()` constructor, kept off `StorageBackend` so the latter stays object-safe.
- **`OpenConfig`** — common open-time knobs (read-only, WAL, page size hint).
- **`BackendError`** — common error surface, including explicit busy,
  transaction-boundary-loss, callback-contract, and persistent poison states.
- **`StorageTransaction`** — exclusive transaction handle passed to
  `with_transaction` closures. The backend commits on `Ok`, rolls back on
  `Err` or panic, and verifies SQLite returned to autocommit before reuse.
  SQLite's authorizer rejects callback-issued `BEGIN`, `COMMIT`, `ROLLBACK`,
  `SAVEPOINT`, `RELEASE`, and `ROLLBACK TO` at parser level. If SQLite itself
  ends the transaction (for example, `INSERT OR ROLLBACK`), the handle records
  sticky boundary loss and rejects every later callback operation.
- **`RusqliteBackend`** — the wired persistent implementation and factory over
  a real `rusqlite::Connection`; the remaining extraction work is threading
  more of `storage.rs` through this boundary. The backend owns SQLite's single
  authorizer slot. `new_with_authorizer` composes a caller policy with the
  scoped-transaction fence; plain `new` explicitly installs an allow policy
  and replaces any callback that existed before ownership transfer. Policy
  decisions are immutable for the backend lifetime: they may not depend on
  time, counters, roles, generations, or any other mutable state (interior
  mutability is limited to observability that cannot influence the result).
  Callback-fence transitions flush SQLite's prepare cache so stale prepare-time
  authorization cannot be reused under a different fence state. A separate
  internal-control phase permits only the backend's outer transaction-control
  actions, so a caller policy may deny raw transaction SQL without disabling
  `with_transaction` cleanup or commit.
- **`MockBackend`** — in-memory mock for testing. Records executed statements + tracks transaction state + answers `user_version` queries. Useful in storage.rs unit tests today, before any frankensqlite migration.

## What the substrate intentionally does NOT ship

- **Refactor of storage.rs.** storage.rs is large and uses
  `rusqlite::Connection` directly throughout. Threading the trait through every
  call site is a multi-week refactor filed as **`wa-2l27x.8.cont.extract`**.
- **Real `FrankenSQLiteBackend` impl.** The feature-gated type is deliberately
  a `NOT_WIRED` compile-time scaffold; every operation except
  `backend_name()` fails. Runtime integration remains blocked on one-runtime
  dependency convergence and native exclusive transaction ownership. Filed
  as **`wa-2l27x.8.cont.frankensqlite`**.
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
- Execute an exclusive scoped transaction — covered by
  `with_transaction` / `with_transaction_dyn` and the borrowed
  `StorageTransaction` handle. The callback never owns the connection itself
  and cannot finish the backend-owned outer boundary.
- Schema versioning — covered by `user_version` /
  `set_user_version`.

cont.extract will extend the trait as it threads storage.rs
through — adding `prepare(sql) -> Statement`, `Row` accessors,
parameter binding traits, etc. The substrate is the *floor*, not
the ceiling.

### Transaction proof boundary

The scoped transaction API is synchronous. It provides exactly-once callback
invocation, fail-fast reentrancy rejection, parser-authoritative transaction
control fencing, commit/rollback finalization, panic cleanup and rethrow, and
persistent quarantine when cleanup cannot prove autocommit. It does **not**
claim async cancellation safety: a thread or process can still be interrupted
outside Rust's unwind path.

The general `execute` and `execute_batch` methods intentionally still expose
raw transaction-control SQL outside a scoped callback because existing
storage migration and recovery code uses that surface. Backend-wide ownership
of those legacy controls is separate follow-up work; callers must not infer
that `with_transaction` serializes an independently opened SQLite connection
or survives process death.

## Tests

Representative tests in `storage_backend_trait::tests` include:

| Test | Invariant |
| ---- | --------- |
| `trait_is_dyn_safe` | `Box<dyn StorageBackend>` compiles + dispatches. |
| `mock_records_executed_statements` | The mock log captures every `execute` call in order. |
| `execute_batch_splits_on_semicolons` | Migration scripts decompose correctly. |
| `transaction_commit_records_committed_state` | Callback success commits and updates the mock witness. |
| `transaction_error_rolls_back_when_not_committed` | Callback error rolls back before returning. |
| `transaction_panic_rolls_back_and_resumes_unwind` | Panic cleanup precedes resuming the original payload. |
| `rusqlite_transaction_authorizer_denies_control_sql_and_rolls_back_prior_writes` | SQLite's parser, not string matching, fences transaction-control variants and comments. |
| `rusqlite_backend_composes_caller_authorizer_with_transaction_fence` | Backend transaction fencing preserves an explicitly transferred application policy. |
| `rusqlite_backend_internal_control_bypasses_only_caller_control_denials` | Caller-issued transaction control stays denied while backend-owned scoped control remains usable. |
| `rusqlite_authorizer_phase_transitions_flush_cached_authorizations` | Identical cached SQL is re-authorized after callback fence transitions. |
| `rusqlite_raw_connection_loan_cannot_disable_later_transaction_fence` | A raw legacy loan cannot leave SQLite's single authorizer slot disabled. |
| `rusqlite_raw_connection_loan_panic_restores_fence_before_resume` | Raw-loan unwind cleanup restores the fence before preserving the original payload. |
| `rusqlite_raw_connection_loan_panic_rolls_back_an_open_transaction` | Raw-loan panic cleanup rolls back an open transaction before permitting reuse. |
| `rusqlite_raw_loan_rollback_failure_preserves_panic_and_quarantines` | Failed raw-loan panic cleanup preserves the original payload and fences every later operation. |
| `rusqlite_reclaimed_connection_retains_the_caller_policy` | Fallible raw reclamation removes backend phases without discarding the transferred caller policy. |
| `rusqlite_automatic_rollback_is_sticky_and_blocks_later_callback_writes` | Automatic boundary loss prevents an autocommit suffix. |
| `rusqlite_deferred_constraint_commit_failure_rolls_back_and_reuses_connection` | Failed `COMMIT` is cleaned up and the proven-autocommit connection is reusable. |
| `rusqlite_rollback_failure_quarantines_every_connection_surface` | Failed cleanup permanently fences the unsafe connection. |
| `rusqlite_backend_transaction_rejects_joined_child_reentrancy_without_deadlock` | A callback-spawned worker fails fast instead of deadlocking on the held connection. |
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
