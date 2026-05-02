//! Convenience helpers over [`StorageBackend`] common patterns.
//!
//! **Bead:** br-ft-l1jgo (slice 2 — wa-2l27x.8.cont.extract.callsite)
//! **Substrate:** ft-qgj81 (multi-column reads + ToSqlValue typed
//! params).
//!
//! ## Why this module
//!
//! storage.rs's wired-pass call-site migration repeats the same
//! 3-5 patterns hundreds of times: `SELECT COUNT(*) FROM <t>`,
//! `SELECT name FROM sqlite_master WHERE type='table' AND name=?`,
//! `PRAGMA <name>`. Shipping these as one-liner helpers means
//! every migrated call site goes from a 4-6 line
//! `prepare`/`query`/`map`/`collect` rusqlite recipe to a
//! single function call.
//!
//! Each helper is pure over [`StorageBackend`] — composes the
//! existing trait methods and produces typed Rust values via
//! [`storage_backend_row_helpers`]. Backends do not need to
//! override these (they default-route through the trait
//! surface).
//!
//! Cross-references:
//! - [`crate::storage_backend_trait`] — the trait the helpers
//!   compose.
//! - [`crate::storage_backend_row_helpers`] — typed extractors
//!   the helpers route through.

use crate::storage_backend_row_helpers::{row_i64, row_string};
use crate::storage_backend_trait::{BackendError, StorageBackend, ToSqlValue};

/// `SELECT COUNT(*) FROM <table>` — returns the row count.
///
/// Identifier-safety check on `table` (ASCII alphanumeric +
/// underscore) so callers can pass operator-supplied table
/// names without worrying about injection.
pub fn count_table(
    backend: &dyn StorageBackend,
    table: &str,
) -> Result<i64, BackendError> {
    if !is_safe_identifier(table) {
        return Err(BackendError::Query(format!(
            "count_table: table name `{table}` is not a safe SQLite identifier"
        )));
    }
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
    let row = backend
        .query_row_strings(&sql, &[])?
        .ok_or_else(|| {
            BackendError::Query(format!(
                "count_table: SELECT COUNT(*) FROM \"{table}\" returned no row"
            ))
        })?;
    row_i64(&row, 0)
}

/// `SELECT COUNT(*) FROM <table> WHERE <where_clause>` — returns
/// the filtered row count. The bead's call-site survey shows
/// this is the second-most-common pattern in storage.rs after
/// the no-filter variant; shipping it as a one-call helper
/// matches the migration density.
///
/// `where_clause` is the SQL expression following `WHERE`
/// (e.g. `"pane_id = ?1 AND severity = ?2"`). `params` binds
/// `?N` placeholders within the clause.
///
/// Identifier safety: only `table` is interpolated into the
/// SQL; `where_clause` is concatenated verbatim. Callers MUST
/// ensure `where_clause` is a literal string from production
/// code — never operator input — and bind every value via
/// `params`. The substrate cannot reasonably parse arbitrary
/// SQL expressions to vet the clause; the safety contract
/// shifts to the caller.
pub fn count_table_where(
    backend: &dyn StorageBackend,
    table: &str,
    where_clause: &str,
    params: &[&str],
) -> Result<i64, BackendError> {
    if !is_safe_identifier(table) {
        return Err(BackendError::Query(format!(
            "count_table_where: table name `{table}` is not a safe SQLite identifier"
        )));
    }
    let sql = format!("SELECT COUNT(*) FROM \"{table}\" WHERE {where_clause}");
    let row = backend.query_row_strings(&sql, params)?.ok_or_else(|| {
        BackendError::Query(format!(
            "count_table_where: SELECT COUNT(*) FROM \"{table}\" \
             WHERE {where_clause} returned no row"
        ))
    })?;
    row_i64(&row, 0)
}

/// `SELECT 1 FROM <table> WHERE <where_clause> LIMIT 1` —
/// returns `true` iff at least one row matches. Cheaper than
/// `count_table_where` when the caller only needs existence
/// (SQLite's planner short-circuits).
pub fn row_exists_where(
    backend: &dyn StorageBackend,
    table: &str,
    where_clause: &str,
    params: &[&str],
) -> Result<bool, BackendError> {
    if !is_safe_identifier(table) {
        return Err(BackendError::Query(format!(
            "row_exists_where: table name `{table}` is not a safe SQLite identifier"
        )));
    }
    let sql = format!(
        "SELECT 1 FROM \"{table}\" WHERE {where_clause} LIMIT 1"
    );
    let row = backend.query_row_strings(&sql, params)?;
    Ok(row.is_some())
}

/// Whether the named table exists in `sqlite_master`.
///
/// Returns `Ok(true)` when the table exists, `Ok(false)`
/// otherwise. Routes through `query_row_strings` with a
/// parameterised query so the table-name parameter is bound
/// safely (no identifier-safety check needed since the value
/// is bound, not interpolated).
pub fn table_exists(
    backend: &dyn StorageBackend,
    table: &str,
) -> Result<bool, BackendError> {
    let row = backend.query_row_strings(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
        &[table],
    )?;
    Ok(row.is_some())
}

/// Read a single PRAGMA value as a string (the most common
/// shape: `PRAGMA user_version`, `PRAGMA page_size`,
/// `PRAGMA journal_mode`).
///
/// Identifier-safety check on `pragma` per the same rule as
/// [`count_table`]. Multi-column PRAGMAs (e.g.
/// `PRAGMA wal_checkpoint(FULL)`) should use
/// [`StorageBackend::query_row_strings`] directly — this
/// helper is for the single-column, no-arg case.
pub fn pragma_value(
    backend: &dyn StorageBackend,
    pragma: &str,
) -> Result<Option<String>, BackendError> {
    if !is_safe_identifier(pragma) {
        return Err(BackendError::Query(format!(
            "pragma_value: pragma name `{pragma}` is not a safe SQLite identifier"
        )));
    }
    let sql = format!("PRAGMA {pragma}");
    let row = backend.query_row_strings(&sql, &[])?;
    match row {
        Some(cells) if !cells.is_empty() => Ok(Some(cells[0].clone())),
        _ => Ok(None),
    }
}

/// `SELECT MAX(<column>) FROM <table>` — returns the max as a
/// string per the substrate's canonical encoding (callers can
/// route through `row_helpers::row_i64` etc to retype).
pub fn max_column(
    backend: &dyn StorageBackend,
    table: &str,
    column: &str,
) -> Result<Option<String>, BackendError> {
    if !is_safe_identifier(table) {
        return Err(BackendError::Query(format!(
            "max_column: table name `{table}` is not a safe SQLite identifier"
        )));
    }
    if !is_safe_identifier(column) {
        return Err(BackendError::Query(format!(
            "max_column: column name `{column}` is not a safe SQLite identifier"
        )));
    }
    let sql = format!("SELECT MAX(\"{column}\") FROM \"{table}\"");
    let row = backend.query_row_strings(&sql, &[])?;
    match row {
        Some(cells) if !cells.is_empty() => {
            // MAX over an empty table returns NULL → empty
            // string per the substrate encoding. Treat as None.
            if cells[0].is_empty() {
                Ok(None)
            } else {
                Ok(Some(cells[0].clone()))
            }
        }
        _ => Ok(None),
    }
}

/// List the names of all tables in the database via
/// `sqlite_master`. Skips internal tables (sqlite_*).
pub fn list_user_tables(
    backend: &dyn StorageBackend,
) -> Result<Vec<String>, BackendError> {
    let rows = backend.query_map_strings(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
        &[],
    )?;
    rows.iter()
        .map(|row| row_string(row, 0))
        .collect()
}

/// Run a typed-parameter `INSERT` and return the affected
/// row count. Convenience for callers that want a one-line
/// INSERT without preparing a statement.
///
/// `sql` should contain `?N` placeholders; `params` binds them
/// in order.
///
/// Note: `query_row_typed` is used for the dispatch path so
/// the substrate's tracing/instrumentation (when wired) sees
/// the call as a query rather than an execute. The
/// distinction matters for backends that count INSERT vs
/// SELECT separately in their telemetry.
pub fn execute_typed(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
) -> Result<(), BackendError> {
    backend.query_row_typed(sql, params)?;
    Ok(())
}

/// SQLite identifier safety: ASCII alphanumeric + underscore.
/// Stricter than SQLite's actual lexer but bullet-proof
/// against injection. Same rule as
/// `crate::storage_backend_converter::is_safe_identifier`.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_backend_trait::{OpenConfig, RusqliteBackend};

    fn backend_with_p_table() -> RusqliteBackend {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT); \
                 INSERT INTO p VALUES (1, 'alpha'); \
                 INSERT INTO p VALUES (2, 'beta'); \
                 INSERT INTO p VALUES (3, 'gamma');",
            )
            .unwrap();
        backend
    }

    #[test]
    fn count_table_returns_row_count() {
        let backend = backend_with_p_table();
        assert_eq!(count_table(&backend, "p").unwrap(), 3);
    }

    #[test]
    fn count_table_returns_zero_on_empty_table() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend.execute_batch("CREATE TABLE empty_t (x INT);").unwrap();
        assert_eq!(count_table(&backend, "empty_t").unwrap(), 0);
    }

    #[test]
    fn count_table_rejects_unsafe_identifier() {
        let backend = backend_with_p_table();
        let err = count_table(&backend, "p; DROP").unwrap_err();
        match err {
            BackendError::Query(msg) => assert!(msg.contains("not a safe")),
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn table_exists_true_for_existing() {
        let backend = backend_with_p_table();
        assert!(table_exists(&backend, "p").unwrap());
    }

    #[test]
    fn table_exists_false_for_missing() {
        let backend = backend_with_p_table();
        assert!(!table_exists(&backend, "nonexistent").unwrap());
    }

    #[test]
    fn pragma_value_reads_user_version() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend.set_user_version(42).unwrap();
        let v = pragma_value(&backend, "user_version").unwrap();
        assert_eq!(v.as_deref(), Some("42"));
    }

    #[test]
    fn pragma_value_reads_page_size() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        let v = pragma_value(&backend, "page_size").unwrap();
        // Default page size is 4096 (or some power of 2);
        // just verify the helper returned a non-empty value.
        assert!(v.is_some(), "page_size pragma must return a value");
        let s = v.unwrap();
        let n: i64 = s.parse().expect("page_size must be an integer");
        assert!(n > 0, "page_size must be positive");
    }

    #[test]
    fn pragma_value_rejects_unsafe_identifier() {
        let backend = backend_with_p_table();
        let err = pragma_value(&backend, "user_version; DROP TABLE p").unwrap_err();
        assert!(matches!(err, BackendError::Query(_)));
    }

    #[test]
    fn max_column_returns_largest_value() {
        let backend = backend_with_p_table();
        let max = max_column(&backend, "p", "id").unwrap();
        assert_eq!(max.as_deref(), Some("3"));
    }

    #[test]
    fn max_column_returns_none_on_empty_table() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend.execute_batch("CREATE TABLE empty_t (x INT);").unwrap();
        let max = max_column(&backend, "empty_t", "x").unwrap();
        assert_eq!(max, None);
    }

    #[test]
    fn max_column_rejects_unsafe_table() {
        let backend = backend_with_p_table();
        let err = max_column(&backend, "p; DROP", "id").unwrap_err();
        assert!(matches!(err, BackendError::Query(_)));
    }

    #[test]
    fn max_column_rejects_unsafe_column() {
        let backend = backend_with_p_table();
        let err = max_column(&backend, "p", "id; DROP").unwrap_err();
        assert!(matches!(err, BackendError::Query(_)));
    }

    #[test]
    fn list_user_tables_returns_all_user_tables_in_alpha_order() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch(
                "CREATE TABLE zebra (x INT); \
                 CREATE TABLE apple (y INT); \
                 CREATE TABLE mango (z INT);",
            )
            .unwrap();
        let tables = list_user_tables(&backend).unwrap();
        assert_eq!(
            tables,
            vec!["apple".to_string(), "mango".to_string(), "zebra".to_string()]
        );
    }

    #[test]
    fn list_user_tables_skips_sqlite_internal() {
        let backend = backend_with_p_table();
        let tables = list_user_tables(&backend).unwrap();
        assert!(tables.contains(&"p".to_string()));
        assert!(!tables.iter().any(|t| t.starts_with("sqlite_")));
    }

    #[test]
    fn execute_typed_routes_through_query_row_typed_for_inserts() {
        let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        backend
            .execute_batch("CREATE TABLE p (id INT, name TEXT);")
            .unwrap();
        execute_typed(
            &backend,
            "INSERT INTO p (id, name) VALUES (?1, ?2)",
            &[ToSqlValue::Integer(7), ToSqlValue::Text("zeta")],
        )
        .unwrap();
        assert_eq!(count_table(&backend, "p").unwrap(), 1);
    }

    #[test]
    fn helpers_dispatch_through_box_dyn() {
        let backend: Box<dyn StorageBackend> = Box::new(backend_with_p_table());
        // Object-safety smoke: every helper accepts &dyn
        // StorageBackend.
        let _ = count_table(&*backend, "p");
        let _ = table_exists(&*backend, "p");
        let _ = list_user_tables(&*backend);
    }

    // ----------------------------------------------------------------
    // br-ft-l1jgo slice 4: count_table_where + row_exists_where.
    // ----------------------------------------------------------------

    #[test]
    fn count_table_where_filters_by_clause() {
        let backend = backend_with_p_table();
        // backend_with_p_table creates 3 rows: id=1/2/3.
        let n = count_table_where(&backend, "p", "id > ?1", &["1"]).unwrap();
        assert_eq!(n, 2);
        let n = count_table_where(&backend, "p", "id = ?1", &["3"]).unwrap();
        assert_eq!(n, 1);
        let n = count_table_where(&backend, "p", "id > ?1", &["100"]).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn count_table_where_returns_zero_on_no_match() {
        let backend = backend_with_p_table();
        let n = count_table_where(
            &backend,
            "p",
            "name = ?1",
            &["nonexistent"],
        )
        .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn count_table_where_rejects_unsafe_table() {
        let backend = backend_with_p_table();
        let err = count_table_where(&backend, "p; DROP", "id = ?1", &["1"]).unwrap_err();
        match err {
            BackendError::Query(msg) => assert!(msg.contains("not a safe")),
            other => panic!("expected Query error, got {other:?}"),
        }
    }

    #[test]
    fn row_exists_where_true_when_match() {
        let backend = backend_with_p_table();
        let exists = row_exists_where(&backend, "p", "id = ?1", &["1"]).unwrap();
        assert!(exists);
    }

    #[test]
    fn row_exists_where_false_when_no_match() {
        let backend = backend_with_p_table();
        let exists = row_exists_where(&backend, "p", "id = ?1", &["999"]).unwrap();
        assert!(!exists);
    }

    #[test]
    fn row_exists_where_rejects_unsafe_table() {
        let backend = backend_with_p_table();
        let err = row_exists_where(&backend, "p; DROP", "id = ?1", &["1"]).unwrap_err();
        assert!(matches!(err, BackendError::Query(_)));
    }

    #[test]
    fn count_table_where_handles_compound_clause() {
        let backend = backend_with_p_table();
        // Compound clause with two parameters.
        let n = count_table_where(
            &backend,
            "p",
            "id >= ?1 AND id <= ?2",
            &["1", "2"],
        )
        .unwrap();
        assert_eq!(n, 2);
    }
}
