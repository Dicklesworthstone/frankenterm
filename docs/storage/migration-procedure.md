# Storage backend migration procedure

**Bead:** `ft-s03ox` (wa-2l27x.8.cont.migration_tool) — scope item 4
("Documented at docs/storage/migration-procedure.md").
**Substrate:** `crates/frankenterm-core/src/storage_backend_converter.rs`
(shipped at `240ee0e36` under br-ft-s03ox substrate-pass).

This document covers the operator procedure for migrating an
existing `ft.db` between [`StorageBackend`][trait]
implementations — most commonly `RusqliteBackend` →
`FrankenSQLiteBackend` once `ft-kcdqp` lands the latter.

The same procedure also covers `RusqliteBackend` → `RusqliteBackend`
clones (for vacuum / WAL-checkpoint snapshots) and the
forward-compat path for any future backend.

[trait]: ../../crates/frankenterm-core/src/storage_backend_trait.rs

## Pre-flight

1. **Stop the daemon writing to `source.db`.** `ft daemon stop`
   on the workspace, or `kill` the writer process. The converter
   reads through `query_map_strings` which is safe under
   concurrent reads, but a concurrent writer mutating the source
   mid-copy would produce a torn snapshot.
2. **Capture `source.db` size + sha256.** Use `wc -c source.db`
   and `shasum -a 256 source.db`. The post-conversion
   `verify_equivalence` step is byte-level for cell content but
   not for file-level layout (page size, free-list, WAL state);
   the source sha is useful for after-the-fact rollback.
3. **Free disk space ≥ 2× source size.** The destination starts
   empty and grows row-by-row; the converter does not stream-
   compact, so the destination is approximately the same size as
   the source post-copy plus the WAL.

## Run the converter

The wired-pass CLI sub-command is `ft storage convert`
(deferred to ft-s03ox cont — currently the converter is invoked
programmatically via the substrate API). The procedural shape:

```rust
use frankenterm_core::storage_backend_trait::{
    OpenConfig, RusqliteBackend, StorageBackend,
};
use frankenterm_core::storage_backend_converter::{
    convert_db, verify_equivalence,
};

// 1. Open source (read-only is safer; the converter only
//    issues SELECT against source).
let mut source_config = OpenConfig::default();
source_config.read_only = true;
let source: Box<dyn StorageBackend> = Box::new(
    RusqliteBackend::open("/path/to/source.db", &source_config)?
);

// 2. Open destination + populate schema via the migration
//    runner. The converter does NOT replicate DDL; the
//    destination must already carry the same schema as the
//    source.
let dest_config = OpenConfig::default();
let dest: Box<dyn StorageBackend> = Box::new(
    RusqliteBackend::open("/path/to/dest.db", &dest_config)?
);
// Run the migration runner against `dest` here:
//   frankenterm_core::storage::migrations::migrate_to_version(
//       <conn>, frankenterm_core::storage::SCHEMA_VERSION,
//   )?;

// 3. Discover the table list (typically by walking
//    sqlite_master.tables on `source`; for now operators pass
//    the explicit list).
let tables: Vec<&str> = vec![
    "panes", "events", "segments", "audit_actions",
    /* ... full table list per the source schema ... */
];

// 4. Drive the copy.
let outcome = convert_db(&*source, &*dest, &tables)?;
println!(
    "converted {} tables, {} rows total",
    outcome.tables.len(),
    outcome.row_total(),
);

// 5. Verify byte-for-byte equivalence (scope item 3).
verify_equivalence(&*source, &*dest, &tables)?;
```

## Wired-pass CLI (deferred)

Once the `ft storage convert` sub-command lands (still under
ft-s03ox), the operator-facing form will be:

```bash
ft storage convert \
    --from rusqlite --to frankensqlite \
    --source /path/to/old.db \
    --dest /path/to/new.db \
    --verify
```

The `--verify` flag runs `verify_equivalence` after the copy
and exits 1 on the first divergence (with a `table:row:column`
pointer + both cell values, per the substrate's error shape).

## Rollback

The converter is non-destructive: source is opened read-only,
destination is a fresh file. To rollback:

1. Stop any process pointed at the destination.
2. Move the original `source.db` into place
   (`mv source.db.before-migration source.db`).
3. Remove the destination (`rm dest.db dest.db-wal
   dest.db-shm`).
4. Restart the daemon pointing at the restored source.

## Verification

The substrate's `verify_equivalence(a, b, tables)` walks every
table named in the slice and asserts that
`SELECT * FROM <t> ORDER BY rowid` produces byte-identical rows
between the two backends. Returns `Ok(())` on full match;
`Err(BackendError::Query(msg))` on the first divergence with a
human-readable pointer.

The check is row-by-row + column-by-column. It does NOT
validate:

- Page-level on-disk layout (different backends pick different
  page sizes).
- WAL state (post-copy WAL is empty; source's WAL has been
  drained by the daemon stop step).
- Index physical ordering (logical row order via `rowid` is
  the contract).

## Known caveats (substrate-pass)

The substrate uses string-canonical column encoding
(`encode_sqlite_value_as_string`):

- **NULL** rounds-trips as the empty string. A column whose
  source contained NULL becomes empty TEXT in the destination
  (the wired-pass cont-bead under ft-qgj81 scope item 3 lands
  the typed Row accessor that distinguishes).
- **BLOB** rounds-trips as `<blob:N bytes>` placeholder text.
  Tables with binary blob columns require the wired-pass typed
  copy path before they can be migrated faithfully — until
  then, any blob-bearing table is operator-rejected by adding
  it to the explicit `tables` slice causes the destination to
  carry the placeholder strings instead of the real bytes.

Tables affected by the blob caveat in the current schema:

- `audit_action_data` — `payload` BLOB
- `secret_scan_reports` — `serialized_findings` BLOB

For these, defer migration until the wired-pass typed copy
path lands (tracked under ft-s03ox cont).

## Cross-references

- Substrate: [`crates/frankenterm-core/src/storage_backend_converter.rs`](../../crates/frankenterm-core/src/storage_backend_converter.rs)
- Trait: [`crates/frankenterm-core/src/storage_backend_trait.rs`](../../crates/frankenterm-core/src/storage_backend_trait.rs)
- Typed-row helpers: [`crates/frankenterm-core/src/storage_backend_row_helpers.rs`](../../crates/frankenterm-core/src/storage_backend_row_helpers.rs) (br-ft-l1jgo)
- Trait surface doc: [`storage-backend-trait.md`](storage-backend-trait.md)
- Bead: ft-s03ox (parent — wa-2l27x.8.cont.migration_tool).
