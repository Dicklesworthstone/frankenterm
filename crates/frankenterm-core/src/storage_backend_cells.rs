//! Typed Row accessor for [`crate::storage_backend_trait`]
//! (ft-qgj81 scope item 3).
//!
//! Substrate slice. cc_1's prior slices shipped:
//!   - Slice 1 (8cb28fcd3): `query_row_strings` / `query_map_strings`
//!     trait methods.
//!   - Slice 2 (2d30a8f6a): `ToSqlValue` parameter-binding enum +
//!     `query_row_typed` / `query_map_typed` default impls routing
//!     through the string path.
//!
//! This slice ships the typed *return* path: a [`SqlCell`] enum
//! mirroring [`crate::storage_backend_trait::ToSqlValue`]'s seven
//! variants but in owned form, a [`Row`] trait the call sites
//! consume, and a [`RowCells`] vector-backed implementation the
//! free-function helpers ([`query_row_cells`] / [`query_map_cells`])
//! return. Dyn-safe shape: every method takes `&self` and returns
//! a borrowed reference or a copy, so `Box<dyn Row>` round-trips
//! cleanly.
//!
//! ## Default-path fidelity
//!
//! [`query_row_cells`] + [`query_map_cells`] route through the
//! *string* trait methods (`query_row_typed`, `query_map_typed`)
//! and parse cells back via [`SqlCell::from_canonical_string`].
//! That parser recovers canonical Integer / Real values, but the
//! string round-trip is still lossy for NULL-vs-empty-text and Blob
//! bytes because the canonical encodings do not carry that information.
//! The wired-pass slice adds native overrides on each backend
//! (`RusqliteBackend` reading through `rusqlite::types::Value` directly,
//! frankensqlite likewise) so the round-trip becomes lossless there.
//! Until then call sites that need NULL fidelity stay on the string path
//! + treat empty as their domain's null marker.

use serde::{Deserialize, Serialize};

// SqlCell migrated to `storage_backend_trait` so it can name the
// trait method signatures `query_row_cells` / `query_map_cells`
// without a circular module dependency. Re-exported here so the
// historical import path `crate::storage_backend_cells::SqlCell`
// still resolves.
pub use crate::storage_backend_trait::SqlCell;
use crate::storage_backend_trait::{BackendError, StorageBackend, ToSqlValue};

/// Dyn-safe accessor over a single row's cells. Implementations
/// supply per-index typed reads + a `cell_count` so the call site
/// can drive width-aware iteration without knowing the underlying
/// container.
pub trait Row {
    /// Number of cells in the row.
    fn cell_count(&self) -> usize;

    /// Borrow the cell at `idx`. Returns `None` when `idx` is
    /// out of range so call sites can pattern-match cleanly.
    fn cell(&self, idx: usize) -> Option<&SqlCell>;

    /// Convenience: forward to `cell(idx).map_or(false, SqlCell::is_null)`.
    fn is_null(&self, idx: usize) -> bool {
        self.cell(idx).is_some_and(SqlCell::is_null)
    }

    /// Convenience: typed Integer read.
    fn get_i64(&self, idx: usize) -> Option<i64> {
        self.cell(idx).and_then(SqlCell::as_i64)
    }

    /// Convenience: typed Real read.
    fn get_f64(&self, idx: usize) -> Option<f64> {
        self.cell(idx).and_then(SqlCell::as_f64)
    }

    /// Convenience: typed Text read.
    fn get_text(&self, idx: usize) -> Option<&str> {
        self.cell(idx).and_then(SqlCell::as_text)
    }

    /// Convenience: typed Blob read.
    fn get_blob(&self, idx: usize) -> Option<&[u8]> {
        self.cell(idx).and_then(SqlCell::as_blob)
    }
}

/// Vec-backed [`Row`] implementation. Returned by
/// [`query_row_cells`] + [`query_map_cells`] and can also be
/// constructed directly from a `Vec<SqlCell>` for tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowCells {
    pub cells: Vec<SqlCell>,
}

impl RowCells {
    #[must_use]
    pub fn new(cells: Vec<SqlCell>) -> Self {
        Self { cells }
    }
}

impl From<Vec<SqlCell>> for RowCells {
    fn from(cells: Vec<SqlCell>) -> Self {
        Self::new(cells)
    }
}

impl Row for RowCells {
    fn cell_count(&self) -> usize {
        self.cells.len()
    }

    fn cell(&self, idx: usize) -> Option<&SqlCell> {
        self.cells.get(idx)
    }
}

/// Read at most one row's typed cells from `backend`. Returns
/// `None` when the query produced no rows.
///
/// Delegates to [`StorageBackend::query_row_cells`] — backends with
/// native cell dispatch (e.g. `RusqliteBackend`) round-trip every
/// SQLite storage class losslessly; backends that fall through the
/// default impl recover canonical numeric cells but still cannot
/// distinguish NULL from empty Text or recover Blob bytes from the
/// canonical placeholder.
pub fn query_row_cells(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
) -> Result<Option<RowCells>, BackendError> {
    Ok(backend.query_row_cells(sql, params)?.map(RowCells::new))
}

/// Read every matching row's typed cells via
/// [`StorageBackend::query_map_cells`]. Same fidelity contract as
/// [`query_row_cells`].
pub fn query_map_cells(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
) -> Result<Vec<RowCells>, BackendError> {
    Ok(backend
        .query_map_cells(sql, params)?
        .into_iter()
        .map(RowCells::new)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_backend_trait::{OpenConfig, RusqliteBackend};

    fn fresh_rusqlite() -> RusqliteBackend {
        RusqliteBackend::open(":memory:", &OpenConfig::default()).expect("open")
    }

    // ----------------------------------------------------------------
    // SqlCell
    // ----------------------------------------------------------------

    #[test]
    fn cell_null_predicates() {
        assert!(SqlCell::Null.is_null());
        assert!(!SqlCell::Integer(0).is_null());
        assert!(!SqlCell::Text(String::new()).is_null());
    }

    #[test]
    fn cell_typed_accessors_only_match_their_variant() {
        let i = SqlCell::Integer(42);
        assert_eq!(i.as_i64(), Some(42));
        assert_eq!(i.as_f64(), None);
        assert_eq!(i.as_text(), None);
        assert_eq!(i.as_blob(), None);

        let f = SqlCell::Real(3.5);
        assert_eq!(f.as_f64(), Some(3.5));
        assert_eq!(f.as_i64(), None);

        let t = SqlCell::Text("hi".to_string());
        assert_eq!(t.as_text(), Some("hi"));
        assert_eq!(t.as_i64(), None);

        let b = SqlCell::Blob(vec![1, 2, 3]);
        assert_eq!(b.as_blob(), Some(&[1, 2, 3][..]));
        assert_eq!(b.as_text(), None);

        let n = SqlCell::Null;
        assert_eq!(n.as_i64(), None);
        assert_eq!(n.as_f64(), None);
        assert_eq!(n.as_text(), None);
        assert_eq!(n.as_blob(), None);
    }

    #[test]
    fn cell_from_canonical_string_empty_is_null() {
        assert!(matches!(SqlCell::from_canonical_string(""), SqlCell::Null));
    }

    #[test]
    fn cell_from_canonical_string_recovers_numeric_values() {
        assert_eq!(SqlCell::from_canonical_string("42"), SqlCell::Integer(42));
        assert_eq!(SqlCell::from_canonical_string("3.5"), SqlCell::Real(3.5));
        let c = SqlCell::from_canonical_string("hello");
        assert_eq!(c.as_text(), Some("hello"));
        assert_eq!(
            SqlCell::from_canonical_string("<blob:4 bytes>").as_text(),
            Some("<blob:4 bytes>")
        );
        assert_eq!(
            SqlCell::from_canonical_string("0042").as_text(),
            Some("0042")
        );
    }

    #[test]
    fn cell_serde_uses_kind_value_tagged_form() {
        let cells = vec![
            SqlCell::Null,
            SqlCell::Integer(7),
            SqlCell::Real(2.5),
            SqlCell::Text("hi".to_string()),
            SqlCell::Blob(vec![0xff]),
        ];
        for cell in &cells {
            let s = serde_json::to_string(cell).unwrap();
            let parsed: SqlCell = serde_json::from_str(&s).unwrap();
            assert_eq!(&parsed, cell);
        }
        let v: serde_json::Value = serde_json::to_value(&cells[1]).unwrap();
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("integer"));
        assert_eq!(v.get("value").and_then(|x| x.as_i64()), Some(7));
    }

    // ----------------------------------------------------------------
    // Row + RowCells
    // ----------------------------------------------------------------

    #[test]
    fn row_cells_count_and_index_in_bounds() {
        let row = RowCells::new(vec![
            SqlCell::Integer(1),
            SqlCell::Text("a".into()),
            SqlCell::Null,
        ]);
        assert_eq!(row.cell_count(), 3);
        assert_eq!(row.get_i64(0), Some(1));
        assert_eq!(row.get_text(1), Some("a"));
        assert!(row.is_null(2));
    }

    #[test]
    fn row_cells_index_out_of_bounds_returns_none() {
        let row = RowCells::new(vec![SqlCell::Integer(1)]);
        assert!(row.cell(99).is_none());
        assert!(!row.is_null(99));
        assert_eq!(row.get_i64(99), None);
        assert_eq!(row.get_text(99), None);
    }

    #[test]
    fn row_cells_typed_accessor_skips_wrong_variant() {
        // Asking for an i64 on a Text cell returns None — the
        // accessors do not implicitly coerce.
        let row = RowCells::new(vec![SqlCell::Text("42".into())]);
        assert_eq!(row.get_i64(0), None);
        assert_eq!(row.get_text(0), Some("42"));
    }

    #[test]
    fn row_cells_dyn_dispatch_works_through_box() {
        let row: Box<dyn Row> = Box::new(RowCells::new(vec![SqlCell::Integer(7)]));
        assert_eq!(row.get_i64(0), Some(7));
    }

    #[test]
    fn row_cells_serde_roundtrip() {
        let row = RowCells::new(vec![
            SqlCell::Integer(1),
            SqlCell::Real(2.5),
            SqlCell::Text("x".into()),
            SqlCell::Blob(vec![1, 2, 3]),
            SqlCell::Null,
        ]);
        let s = serde_json::to_string(&row).unwrap();
        let parsed: RowCells = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, row);
    }

    // ----------------------------------------------------------------
    // query_row_cells / query_map_cells (default-path fidelity)
    // ----------------------------------------------------------------

    #[test]
    fn query_row_cells_returns_none_on_empty_select() {
        let backend = fresh_rusqlite();
        let row = query_row_cells(&backend, "SELECT 1 WHERE 1 = 0", &[]).unwrap();
        assert!(row.is_none());
    }

    #[test]
    fn query_row_cells_returns_text_for_text_column_default_path() {
        let backend = fresh_rusqlite();
        let row = query_row_cells(&backend, "SELECT 'hello' AS msg", &[])
            .unwrap()
            .expect("row");
        assert_eq!(row.cell_count(), 1);
        assert_eq!(row.get_text(0), Some("hello"));
    }

    #[test]
    fn query_map_cells_returns_one_rowcells_per_match() {
        let backend = fresh_rusqlite();
        backend
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
                 INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .unwrap();
        let rows = query_map_cells(&backend, "SELECT id, body FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.cell_count(), 2);
        }
    }

    #[test]
    fn query_map_cells_empty_table_returns_empty_vec() {
        let backend = fresh_rusqlite();
        backend
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        let rows = query_map_cells(&backend, "SELECT x FROM t", &[]).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_row_cells_propagates_backend_error() {
        let backend = fresh_rusqlite();
        let err = query_row_cells(&backend, "SELECT * FROM does_not_exist", &[]).unwrap_err();
        // The substrate's BackendError is opaque here; we just
        // assert the error path exists rather than match a specific
        // discriminant the substrate may evolve.
        let _ = format!("{err}");
    }
}
