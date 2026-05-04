# StorageBackend migration guide (ft-l1jgo)

Operator-facing guide to the per-pattern recipes the wired-pass
call-site migration in `storage.rs` follows. Pairs with
[`scripts/storage_backend_callsites.py`][analyzer] which inventories
the live callsite mix and points at this guide via the
`replacement_module::function` field of every pattern row.

[analyzer]: ../../scripts/storage_backend_callsites.py

## Why the migration exists

`storage.rs` (17.6k LOC at the time of writing) talks to SQLite
through `rusqlite` directly. The `wa-2l27x.8` epic introduced a
[`StorageBackend`][trait] trait abstraction so the same call sites
can target either the rusqlite-backed path or the future
`FrankenSQLiteBackend` (gated under `--features frankensqlite-backend`,
landing under [`ft-kcdqp`][kcdqp]). The substrate is in place; the
remaining work is converting each call site to the trait surface.

[trait]: ../../crates/frankenterm-core/src/storage_backend_trait.rs
[kcdqp]: https://github.com/frankenterm/frankenterm/issues?q=ft-kcdqp

The migration runs **per submodule** — one of `storage/migrations`,
`storage/handle`, `storage/timeline`, `storage/export`, etc. at a
time. Each submodule's commit migrates its callsites + verifies its
tests still pass. Do not mix submodules in a single commit; the
migration analyzer's drift check cannot tell you which submodule a
regression came from when commits straddle.

## Workflow

1. Run the analyzer to baseline the callsite mix:

   ```sh
   python3 scripts/storage_backend_callsites.py
   # writes docs/storage/callsite-migration-plan.json
   ```

2. Pick a submodule. Open it next to this guide.

3. For each direct-rusqlite pattern in the submodule, find the
   matching recipe below and apply it. Compile + run the
   submodule's tests after each pattern cluster lands.

4. Re-run the analyzer. The `total_callsites` count should
   decrease; the `migration_priority` list reorders to reflect the
   new top-frequency patterns.

5. When `0 direct rusqlite::Connection references` is reached the
   bead's acceptance is met and the migration is done.

## Recipes

### Single-row scalar reads (`conn.query_row(...)`)

**Before:**

```rust
let count: i64 = conn.query_row(
    "SELECT COUNT(*) FROM panes WHERE state = ?",
    params!["active"],
    |row| row.get(0),
)?;
```

**After:**

```rust
use frankenterm_core::storage_backend_trait::ToSqlValue;

let row = backend.query_row_typed(
    "SELECT COUNT(*) FROM panes WHERE state = ?",
    &[ToSqlValue::Text("active")],
)?;
let count: i64 = row
    .as_ref()
    .and_then(|cells| cells.first())
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);
```

When NULL fidelity matters, use the typed Row accessor from
[`storage_backend_cells`][cells] (the wired-pass override on
`RusqliteBackend` lands lossless reads):

[cells]: ../../crates/frankenterm-core/src/storage_backend_cells.rs

```rust
use frankenterm_core::storage_backend_cells::{query_row_cells, Row};

let row = query_row_cells(backend, sql, &params)?;
let count = row.and_then(|r| r.get_i64(0));
```

### Multi-row reads (`conn.query_map(...)`)

**Before:**

```rust
let mut stmt = conn.prepare("SELECT id, body FROM notes ORDER BY id")?;
let rows = stmt
    .query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?;
```

**After:**

```rust
use frankenterm_core::storage_backend_cells::{query_map_cells, Row};

let rows = query_map_cells(backend, "SELECT id, body FROM notes ORDER BY id", &[])?;
let collected: Vec<(i64, String)> = rows
    .iter()
    .filter_map(|r| Some((r.get_i64(0)?, r.get_text(1)?.to_string())))
    .collect();
```

If you have many such call sites with the same row shape, factor a
helper using [`storage_backend_row_helpers::RowReader`][rr]:

[rr]: ../../crates/frankenterm-core/src/storage_backend_row_helpers.rs

```rust
use frankenterm_core::storage_backend_row_helpers::RowReader;

let collected: Vec<(i64, String)> = rows
    .iter()
    .filter_map(|cells| {
        let r = RowReader::new(&cells.cells);
        Some((r.i64(0)?, r.string(1)?))
    })
    .collect();
```

### DDL / multi-statement execution (`conn.execute_batch(...)`)

The trait's `execute_batch` is identical in shape — just retarget
the receiver:

```rust
backend.execute_batch(
    "CREATE TABLE foo (...); CREATE INDEX foo_idx ON foo(...);"
)?;
```

### Single-statement execute (`conn.execute(...)`)

**Before:**

```rust
conn.execute("DELETE FROM foo WHERE id = ?", params![id])?;
```

**After:** use [`storage_backend_helpers::execute_typed`][helpers]
which wraps the trait's bind path:

[helpers]: ../../crates/frankenterm-core/src/storage_backend_helpers.rs

```rust
use frankenterm_core::storage_backend_helpers::execute_typed;
use frankenterm_core::storage_backend_trait::ToSqlValue;

execute_typed(
    backend,
    "DELETE FROM foo WHERE id = ?",
    &[ToSqlValue::Integer(id)],
)?;
```

### `prepare` / `prepare_cached`

The substrate exposes two distinct migration paths depending on
how the prepared statement is used:

#### Pattern A — single-shot prepare + `query_row` / `query_map`

Migrate to [`StorageBackend::query_row_typed`][trait] /
[`StorageBackend::query_map_typed`][trait]. The prepare step is
internal to those methods; no separate Statement abstraction is
needed.

**Before:**

```rust
let mut stmt = conn.prepare(
    "SELECT name FROM accounts WHERE id = ?1",
)?;
let name: Option<String> = stmt
    .query_row(params![id], |row| row.get(0))
    .optional()?;
```

**After:**

```rust
use frankenterm_core::storage_backend_trait::ToSqlValue;

let row = backend.query_row_typed(
    "SELECT name FROM accounts WHERE id = ?1",
    &[ToSqlValue::Integer(id)],
)?;
let name = row.and_then(|cells| cells.into_iter().next());
```

#### Pattern B — `prepare_cached` + loop `execute` (bulk insert/update)

Migrate to [`StorageBackend::execute_many`][trait]. The trait
method takes a `&[Vec<ToSqlValue<'_>>]` of param rows and submits
each in turn; `RusqliteBackend` overrides the default impl with
`prepare_cached` so the prepare-once optimization is preserved.

**Before:**

```rust
let mut stmt = tx.prepare_cached(
    "INSERT INTO usage_metrics (timestamp, count) VALUES (?1, ?2)",
)?;
for record in records {
    stmt.execute(params![record.timestamp, record.count])?;
}
```

**After:**

```rust
use frankenterm_core::storage_backend_trait::ToSqlValue;

let rows: Vec<Vec<ToSqlValue<'_>>> = records
    .iter()
    .map(|r| vec![
        ToSqlValue::Integer(r.timestamp),
        ToSqlValue::Integer(r.count),
    ])
    .collect();
backend.execute_many(
    "INSERT INTO usage_metrics (timestamp, count) VALUES (?1, ?2)",
    &rows,
)?;
```

For atomic-batch semantics (rollback on any error), wrap the
`execute_many` call between explicit `BEGIN`/`COMMIT`:

```rust
backend.execute("BEGIN")?;
match backend.execute_many(sql, &rows) {
    Ok(count) => {
        backend.execute("COMMIT")?;
        Ok(count)
    }
    Err(e) => {
        // Best-effort ROLLBACK — even if it fails (e.g., the
        // connection is already in a bad state), propagate the
        // ORIGINAL error rather than masking it with a rollback
        // failure. SQLite auto-rolls back on most failure modes
        // anyway; the explicit ROLLBACK here closes the
        // statement-level rollback that constraint violations
        // perform without ending the surrounding transaction.
        let _ = backend.execute("ROLLBACK");
        Err(e)
    }
}
```

Note: do NOT propagate ROLLBACK errors with `?` — that masks
the original failure. Do NOT skip the COMMIT error: a failed
COMMIT means the data was NOT persisted, so the caller needs to
know. SQLite auto-rolls back on COMMIT failure, so no explicit
ROLLBACK is required after a failed COMMIT.

The trait substrate landed under [`ft-qgj81`][qgj81] slice 5.

[qgj81]: https://github.com/frankenterm/frankenterm/issues?q=ft-qgj81

### PRAGMA reads (`conn.pragma_query(...)`)

**Before:**

```rust
let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
```

**After:** [`storage_backend_helpers::pragma_value`][helpers] takes
a name + (optional) parser:

```rust
use frankenterm_core::storage_backend_helpers::pragma_value;

let user_version: i64 = pragma_value(backend, "user_version")?
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);
```

### Transactions (`conn.transaction()`)

**Currently blocked.** Trait surface includes transaction lifecycle
methods, but the call-site borrowing pattern (`Transaction` as a
typed value held across the closure) needs a closure-based helper
the substrate has not yet shipped. Track this under [`ft-qgj81`'s
follow-on slices][qgj81]. Defer migration of these call sites.

### Parameter binding (`rusqlite::params!` / `params_from_iter`)

**Before:**

```rust
conn.execute(
    "INSERT INTO foo (id, name, blob) VALUES (?, ?, ?)",
    params![id, name, blob],
)?;
```

**After:** convert each argument to a [`ToSqlValue`][tosql]:

[tosql]: ../../crates/frankenterm-core/src/storage_backend_trait.rs

```rust
use frankenterm_core::storage_backend_helpers::execute_typed;
use frankenterm_core::storage_backend_trait::ToSqlValue;

execute_typed(
    backend,
    "INSERT INTO foo (id, name, blob) VALUES (?, ?, ?)",
    &[
        ToSqlValue::Integer(id),
        ToSqlValue::Text(&name),
        ToSqlValue::Blob(&blob),
    ],
)?;
```

`ToSqlValue` carries the seven-variant shape SQLite needs;
`From<i64>` / `From<&str>` / `From<&[u8]>` impls let many callers
write `id.into()`, `name.as_str().into()`, etc.

### Indexed row reads (`row.get(0)` / `row.get::<_, T>(0)`)

These appear inside `query_map` closures the second-row recipe
above already migrated. After applying that recipe, consume rows
via [`RowReader`][rr] or the typed accessor on
[`storage_backend_cells::RowCells`][cells]:

```rust
let r = RowCells::new(cells_vec);
let id   = r.get_i64(0)?;
let body = r.get_text(1)?;
```

### Direct `rusqlite::Connection::open`

**Before:**

```rust
let conn = rusqlite::Connection::open(&path)?;
```

**After:**

```rust
use frankenterm_core::storage_backend_trait::{OpenConfig, RusqliteBackend, StorageBackend};

let backend = RusqliteBackend::open(&path, &OpenConfig::default())?;
```

`OpenConfig` carries `wal_mode`, `busy_timeout_ms`, and other
operator-facing knobs the trait surfaces uniformly across backends.

## Non-recipes (defer until trait extends)

The analyzer's `missing_substrate` list reflects the current state
of the trait. As of this writing one pattern is blocked:

- `conn_transaction` — needs a closure-based transaction helper.
  Tracked under [`ft-qgj81`'s follow-on slices][qgj81].

`conn_prepare` / `conn_prepare_cached` were unblocked by the
`execute_many` substrate (slice 5); see the prepare/prepare_cached
recipe above.

When the transaction helper lands the analyzer's
`missing_substrate` drops to empty and the migration can complete.

## Testing

After each pattern cluster:

1. `cargo test -p frankenterm-core --lib --no-default-features storage`.
2. `cargo test -p frankenterm-core` for the full integration suite.
3. Re-run `python3 scripts/storage_backend_callsites.py` and
   confirm the diff in `docs/storage/callsite-migration-plan.json`
   matches the cluster you migrated.
4. The CI lane runs `--check` on the analyzer; PR gates fail when
   the on-disk plan drifts unexpectedly. Refresh the plan in the
   same commit as the migration so reviewers see the new baseline.

## Cross-references

- `ft-mbs4e` — RusqliteBackend + scalar trait surface (slice 1).
- `ft-qgj81` — extended trait surface (multi-column, ToSqlValue,
  Row accessor); landed slices 1–3 + analyzer-flagged Statement /
  Transaction follow-ons.
- `ft-l1jgo` — this bead. Per-submodule call-site migration; the
  analyzer + this guide are the operator workflow.
- `ft-kcdqp` — `FrankenSQLiteBackend`. Drops in alongside
  `RusqliteBackend` once the call-site migration is complete +
  `frankensqlite` Phase 5+ ships.
- `ft-s03ox` — backend-to-backend `.db` converter. Composes against
  the same trait surface as the migration; reference CLI ships at
  `crates/frankenterm-core/examples/storage_convert.rs`.
- `ft-giisk` — side-by-side rusqlite vs frankensqlite benchmarks.
  Same harness consumes both backends through the trait.
