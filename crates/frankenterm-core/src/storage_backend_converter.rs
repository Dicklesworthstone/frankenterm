//! Backend-to-backend `.db` converter (br-ft-s03ox substrate-pass).
//!
//! **Bead:** `ft-s03ox` / wa-2l27x.8.cont.migration_tool.
//!
//! Reads source rows via [`StorageBackend::query_map_cells`]
//! and writes destination rows via typed INSERT bindings. Pure
//! over the trait — works between any pair of backends that
//! both implement the substrate ([`RusqliteBackend`] today,
//! `FrankenSQLiteBackend` once `ft-kcdqp` lands).
//!
//! ## Substrate vs wired-pass split
//!
//! The bead is "blocked on cont.frankensqlite" only for the
//! end-to-end usefulness — the converter LOGIC works between
//! any two `Box<dyn StorageBackend>` today. This module ships
//! the logic so the CLI binding (`ft storage convert
//! --from rusqlite --to frankensqlite <path>`, scope item 1)
//! is a one-line wire-up when frankensqlite is real.
//!
//! Until then, the converter is exercised between two
//! [`RusqliteBackend`] instances (a real "clone" use case —
//! useful for vacuum / WAL-checkpoint snapshots) + against
//! test-local mock backends in unit tests.
//!
//! ## What this module ships
//!
//! - [`ConvertOutcome`] — counts rows + tables copied, plus a
//!   per-table digest used for verification.
//! - [`convert_db`] — top-level driver.
//! - [`copy_table`] — single-table copy used by [`convert_db`]
//!   and exposed for callers that want partial copies.
//! - [`verify_equivalence`] — compares two backends column-by-
//!   column, returning the first divergence (or `Ok(())` on
//!   match). Implements scope item 3 (row-count + content-hash
//!   equivalence).
//!
//! ## What this module deliberately does NOT ship
//!
//! - The `ft storage convert` CLI sub-command — wired-pass.
//! - Schema/index/trigger replication — the substrate-pass
//!   copies row data only; the destination is expected to
//!   already carry the same DDL (the wired-pass populates the
//!   destination via the migration runner before invoking the
//!   converter).
//! - Cross-backend type-coercion (TEXT→INTEGER etc.) — each value is
//!   copied through the [`SqlCell`] / [`ToSqlValue`] storage-class
//!   surface. Native backends preserve NULL and BLOB bytes; backends that
//!   only implement the lossy default cell path must override it before
//!   participating in fidelity-sensitive conversion.
//!
//! Cross-references:
//! - `crates/frankenterm-core/src/storage_backend_trait.rs`
//!   (StorageBackend trait + RusqliteBackend).
//! - `crates/frankenterm-core/src/storage_backend_row_helpers.rs`
//!   (typed extractors used by `verify_equivalence`).
//! - br-ft-l1jgo (call-site migration consumer of the same
//!   StorageBackend trait surface).

use crate::storage_backend_trait::{BackendError, SqlCell, StorageBackend, ToSqlValue};

/// Result of a successful convert run. Counts what landed +
/// surfaces a per-table digest the caller can pin in tests or
/// release artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertOutcome {
    /// Names of tables copied, in source order.
    pub tables: Vec<String>,
    /// Per-table row counts, parallel to [`Self::tables`].
    pub rows_per_table: Vec<usize>,
    /// Total rows copied (sum of [`Self::rows_per_table`]).
    pub total_rows: usize,
}

impl ConvertOutcome {
    /// Convenience: row count summed across all tables.
    #[must_use]
    pub fn row_total(&self) -> usize {
        self.total_rows
    }
}

/// Copy a single table from `source` to `dest`. Returns the
/// number of rows copied. Does NOT create the table on the
/// destination — the caller must run the migration runner
/// against `dest` before invoking this.
///
/// Quoting policy: identifiers (table names) are double-quoted
/// per SQLite conventions. Values are routed through `?N`
/// positional parameter binding to avoid string-injection.
///
/// Values are read as [`SqlCell`] and bound as [`ToSqlValue`], preserving
/// SQLite storage classes on native backends. This is intentionally stricter
/// than the historical string pipeline, which flattened NULL to empty text and
/// replaced BLOB contents with a size placeholder.
pub fn copy_table(
    source: &dyn StorageBackend,
    dest: &dyn StorageBackend,
    table: &str,
) -> Result<usize, BackendError> {
    if !is_safe_identifier(table) {
        return Err(BackendError::Query(format!(
            "table name `{table}` contains characters not allowed in SQLite \
             identifiers (substrate-pass requires plain alphanumeric + underscore)"
        )));
    }
    require_lossless_cells(source, "source")?;
    require_lossless_cells(dest, "destination")?;

    // Discover columns by inspecting the first row's column
    // metadata. We use PRAGMA table_info(<table>) which returns
    // (cid, name, type, notnull, default, pk).
    let pragma_sql = format!("PRAGMA table_info(\"{table}\")");
    let column_rows = source.query_map_strings(&pragma_sql, &[])?;
    if column_rows.is_empty() {
        return Err(BackendError::Query(format!(
            "table `{table}` does not exist on source backend or has no columns"
        )));
    }
    let column_names: Vec<String> = column_rows
        .iter()
        .map(|row| row.get(1).cloned().unwrap_or_default())
        .collect();
    let column_count = column_names.len();
    let column_list = column_names
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholder_list = (1..=column_count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let select_sql = format!("SELECT {column_list} FROM \"{table}\"");
    let insert_sql = format!("INSERT INTO \"{table}\" ({column_list}) VALUES ({placeholder_list})");

    let rows = source.query_map_cells(&select_sql, &[])?;
    let mut param_rows = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != column_count {
            return Err(BackendError::Query(format!(
                "table `{table}`: row width {} mismatches column count {}",
                row.len(),
                column_count,
            )));
        }
        param_rows.push(row.iter().map(sql_cell_to_owned_value).collect());
    }
    let inserted = dest.execute_many(&insert_sql, &param_rows)?;
    if inserted != param_rows.len() {
        return Err(BackendError::Query(format!(
            "table `{table}`: inserted {inserted} rows but expected {}",
            param_rows.len()
        )));
    }
    Ok(inserted)
}

/// Drive a full source→dest copy. Caller has already populated
/// `dest`'s schema (typically by running the migration runner
/// against an empty `dest`).
///
/// `tables` is the explicit list to copy in order. The wired-
/// pass CLI computes this list by walking
/// `sqlite_master.tables` on the source; the substrate-pass
/// keeps the discovery step explicit so tests can pin behavior
/// without touching `sqlite_master`.
pub fn convert_db(
    source: &dyn StorageBackend,
    dest: &dyn StorageBackend,
    tables: &[&str],
) -> Result<ConvertOutcome, BackendError> {
    let mut tables_out = Vec::with_capacity(tables.len());
    let mut rows_per_table = Vec::with_capacity(tables.len());
    let mut total_rows = 0;
    for &table in tables {
        let rows = copy_table(source, dest, table)?;
        tables_out.push(table.to_string());
        rows_per_table.push(rows);
        total_rows += rows;
    }
    Ok(ConvertOutcome {
        tables: tables_out,
        rows_per_table,
        total_rows,
    })
}

/// Verify row-count + content-hash equivalence between two
/// backends across the named tables (scope item 3 of ft-s03ox).
///
/// Returns `Ok(())` when every named table's typed
/// `query_map_cells("SELECT * FROM <t> ORDER BY rowid", &[])` output is
/// byte-identical between the two backends. Returns the first divergence as
/// `BackendError::Query` carrying the table name + row index + column index.
///
/// Ordering also assumes a rowid table (`ORDER BY rowid`); `WITHOUT ROWID`
/// tables are out of scope for this verifier.
pub fn verify_equivalence(
    a: &dyn StorageBackend,
    b: &dyn StorageBackend,
    tables: &[&str],
) -> Result<(), BackendError> {
    for &table in tables {
        if !is_safe_identifier(table) {
            return Err(BackendError::Query(format!(
                "table name `{table}` is not a safe SQLite identifier"
            )));
        }
    }
    require_lossless_cells(a, "source")?;
    require_lossless_cells(b, "destination")?;

    for &table in tables {
        let sql = format!("SELECT * FROM \"{table}\" ORDER BY rowid");
        let rows_a = a.query_map_cells(&sql, &[])?;
        let rows_b = b.query_map_cells(&sql, &[])?;
        if rows_a.len() != rows_b.len() {
            return Err(BackendError::Query(format!(
                "table `{table}` row-count mismatch: source has {} rows, \
                 dest has {} rows",
                rows_a.len(),
                rows_b.len(),
            )));
        }
        for (row_idx, (row_a, row_b)) in rows_a.iter().zip(rows_b.iter()).enumerate() {
            if row_a.len() != row_b.len() {
                return Err(BackendError::Query(format!(
                    "table `{table}` row {row_idx}: column-count mismatch \
                     ({} vs {})",
                    row_a.len(),
                    row_b.len(),
                )));
            }
            for (col_idx, (cell_a, cell_b)) in row_a.iter().zip(row_b.iter()).enumerate() {
                if cell_a != cell_b {
                    let cell_a = describe_cell(cell_a);
                    let cell_b = describe_cell(cell_b);
                    return Err(BackendError::Query(format!(
                        "table `{table}` row {row_idx} column {col_idx}: \
                         source has `{cell_a}`, dest has `{cell_b}`"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn sql_cell_to_owned_value(cell: &SqlCell) -> ToSqlValue<'static> {
    match cell {
        SqlCell::Null => ToSqlValue::Null,
        SqlCell::Integer(i) => ToSqlValue::Integer(*i),
        SqlCell::Real(f) => ToSqlValue::Real(*f),
        SqlCell::Text(s) => ToSqlValue::OwnedText(s.clone()),
        SqlCell::Blob(b) => ToSqlValue::OwnedBlob(b.clone()),
    }
}

fn describe_cell(cell: &SqlCell) -> String {
    match cell {
        SqlCell::Null => "NULL".to_string(),
        SqlCell::Integer(i) => i.to_string(),
        SqlCell::Real(f) => f.to_string(),
        SqlCell::Text(s) => s.clone(),
        SqlCell::Blob(b) => format!("<blob:{} bytes>", b.len()),
    }
}

fn require_lossless_cells(backend: &dyn StorageBackend, role: &str) -> Result<(), BackendError> {
    if backend.supports_lossless_cells() {
        return Ok(());
    }
    Err(BackendError::Query(format!(
        "{role} backend `{}` does not advertise lossless SqlCell/ToSqlValue support; \
         refusing storage conversion because trait-default cell paths flatten BLOB bytes and NULL values",
        backend.backend_name()
    )))
}

/// SQLite identifier safety: ASCII alphanumeric + underscore.
/// Stricter than SQLite's actual lexer (which accepts a wider
/// range under quoting) but sufficient for the substrate-pass
/// use case + bullet-proof against injection.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_backend_trait::{MockBackend, OpenConfig, RusqliteBackend};

    fn populated_source() -> RusqliteBackend {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT, weight REAL); \
                 INSERT INTO p VALUES (1, 'alpha', 1.5); \
                 INSERT INTO p VALUES (2, 'beta', 2.5); \
                 INSERT INTO p VALUES (3, 'gamma', 3.5);",
            )
            .unwrap();
        backend
    }

    fn empty_dest_with_schema() -> RusqliteBackend {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch("CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT, weight REAL);")
            .unwrap();
        backend
    }

    #[test]
    fn copy_table_round_trips_three_rows() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        let n = copy_table(&source, &dest, "p").unwrap();
        assert_eq!(n, 3);
        let rows = dest
            .query_map_strings("SELECT id, name, weight FROM p ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            vec!["1".to_string(), "alpha".to_string(), "1.5".to_string()]
        );
        assert_eq!(
            rows[2],
            vec!["3".to_string(), "gamma".to_string(), "3.5".to_string()]
        );
    }

    #[test]
    fn convert_db_sums_row_counts_per_table() {
        let source = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        source
            .execute_batch(
                "CREATE TABLE t1 (x INT); \
                 CREATE TABLE t2 (y INT); \
                 INSERT INTO t1 VALUES (1), (2); \
                 INSERT INTO t2 VALUES (10), (20), (30);",
            )
            .unwrap();
        let dest = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        dest.execute_batch(
            "CREATE TABLE t1 (x INT); \
             CREATE TABLE t2 (y INT);",
        )
        .unwrap();

        let out = convert_db(&source, &dest, &["t1", "t2"]).unwrap();
        assert_eq!(out.tables, vec!["t1".to_string(), "t2".to_string()]);
        assert_eq!(out.rows_per_table, vec![2, 3]);
        assert_eq!(out.total_rows, 5);
        assert_eq!(out.row_total(), 5);
    }

    #[test]
    fn verify_equivalence_passes_on_byte_identical_clones() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        copy_table(&source, &dest, "p").unwrap();
        verify_equivalence(&source, &dest, &["p"]).unwrap();
    }

    #[test]
    fn verify_equivalence_reports_row_count_divergence() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        // Copy only 2 of 3 rows manually so the dest is short.
        dest.execute_batch(
            "INSERT INTO p VALUES (1, 'alpha', 1.5); \
             INSERT INTO p VALUES (2, 'beta', 2.5);",
        )
        .unwrap();
        let err = verify_equivalence(&source, &dest, &["p"]).unwrap_err();
        match err {
            BackendError::Query(msg) => assert!(msg.contains("row-count mismatch")),
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn verify_equivalence_reports_cell_divergence_with_coordinates() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        dest.execute_batch(
            "INSERT INTO p VALUES (1, 'alpha', 1.5); \
             INSERT INTO p VALUES (2, 'BETA-DIFF', 2.5); \
             INSERT INTO p VALUES (3, 'gamma', 3.5);",
        )
        .unwrap();
        let err = verify_equivalence(&source, &dest, &["p"]).unwrap_err();
        match err {
            BackendError::Query(msg) => {
                assert!(msg.contains("row 1 column 1"));
                assert!(msg.contains("beta"));
                assert!(msg.contains("BETA-DIFF"));
            }
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn copy_table_rejects_unsafe_identifier() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        let err = copy_table(&source, &dest, "p; DROP TABLE p").unwrap_err();
        match err {
            BackendError::Query(msg) => assert!(msg.contains("not allowed")),
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn copy_table_rejects_missing_table() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        let err = copy_table(&source, &dest, "nonexistent").unwrap_err();
        match err {
            BackendError::Query(msg) => assert!(msg.contains("does not exist")),
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn storage_converter_rejects_lossy_default_cell_backend() {
        let source = MockBackend::new();
        let dest = MockBackend::new();
        let err = copy_table(&source, &dest, "p").unwrap_err();
        match err {
            BackendError::Query(msg) => {
                assert!(msg.contains("source backend `mock`"));
                assert!(msg.contains("lossless SqlCell/ToSqlValue support"));
            }
            other => panic!("expected Query error, got {other:?}"),
        }

        let err = verify_equivalence(&source, &dest, &["p"]).unwrap_err();
        match err {
            BackendError::Query(msg) => {
                assert!(msg.contains("source backend `mock`"));
                assert!(msg.contains("lossless SqlCell/ToSqlValue support"));
            }
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn convert_db_round_trips_preserves_byte_identity() {
        let source = populated_source();
        let dest = empty_dest_with_schema();
        let _ = convert_db(&source, &dest, &["p"]).unwrap();
        // The post-convert verify_equivalence must pass.
        verify_equivalence(&source, &dest, &["p"]).unwrap();
    }

    #[test]
    fn is_safe_identifier_accepts_alphanumeric_underscore() {
        assert!(is_safe_identifier("p"));
        assert!(is_safe_identifier("pane_log"));
        assert!(is_safe_identifier("t123_456"));
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("p p"));
        assert!(!is_safe_identifier("p; DROP"));
        assert!(!is_safe_identifier("p--comment"));
        assert!(!is_safe_identifier("p\""));
    }
}
