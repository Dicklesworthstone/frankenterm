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
//! That is **lossy** on the NULL / empty-text and Integer / Real /
//! Blob distinctions: the string round-trip cannot tell them apart
//! once a backend has rendered them. The wired-pass slice will add
//! native overrides on each backend (`RusqliteBackend` reading
//! through `rusqlite::types::Value` directly, frankensqlite
//! likewise) so the round-trip becomes lossless. Until then call
//! sites that need NULL fidelity stay on the string path + treat
//! empty as their domain's null marker.

use serde::{Deserialize, Serialize};

use crate::storage_backend_trait::{BackendError, StorageBackend, ToSqlValue};

/// Owned typed cell. Mirrors [`ToSqlValue`]'s shape but with all
/// variants in `'static` lifetime so cells flow through dyn
/// boundaries without lifetime juggling.
///
/// SQLite's storage classes are NULL / INTEGER / REAL / TEXT /
/// BLOB; we keep the surface tight to those five.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum SqlCell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlCell {
    /// True when the cell is a SQL NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Borrow as `i64` when the cell is an Integer.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Borrow as `f64` when the cell is a Real (NOT an Integer
    /// promoted to float — call sites pass the explicit Integer
    /// variant when they want one).
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Real(f) => Some(*f),
            _ => None,
        }
    }

    /// Borrow the text when the cell is Text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Borrow the blob when the cell is Blob.
    #[must_use]
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Self::Blob(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Parse a cell from the canonical string encoding the default
    /// trait path uses ([`ToSqlValue::to_canonical_string`]). The
    /// roundtrip is lossy: every cell ends up as Text or Null;
    /// integer / real / blob fidelity comes back via the wired-
    /// pass native overrides.
    #[must_use]
    pub fn from_canonical_string(raw: &str) -> Self {
        if raw.is_empty() {
            // Mirrors the default path's NULL encoding.
            // Distinguishing NULL from empty-text needs the native
            // override; this matches `to_canonical_string`'s rule
            // that NULL renders as the empty string.
            return Self::Null;
        }
        Self::Text(raw.to_string())
    }
}

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

/// Read at most one row's typed cells from `backend`. Default-path
/// fidelity caveats apply (every non-empty cell comes back as
/// Text). Returns `None` when the query produced no rows.
pub fn query_row_cells(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
) -> Result<Option<RowCells>, BackendError> {
    let row = backend.query_row_typed(sql, params)?;
    Ok(row.map(|cells| {
        RowCells::new(
            cells
                .into_iter()
                .map(|raw| SqlCell::from_canonical_string(&raw))
                .collect(),
        )
    }))
}

/// Read every matching row's typed cells. Same default-path
/// caveats as [`query_row_cells`].
pub fn query_map_cells(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
) -> Result<Vec<RowCells>, BackendError> {
    let rows = backend.query_map_typed(sql, params)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            RowCells::new(
                row.into_iter()
                    .map(|raw| SqlCell::from_canonical_string(&raw))
                    .collect(),
            )
        })
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
        assert!(matches!(
            SqlCell::from_canonical_string(""),
            SqlCell::Null
        ));
    }

    #[test]
    fn cell_from_canonical_string_nonempty_is_text() {
        let c = SqlCell::from_canonical_string("hello");
        assert_eq!(c.as_text(), Some("hello"));
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
        // Default path stringifies — Integer / Text round-trip
        // both come back as Text. Documented limitation; the
        // wired-pass native override fixes it.
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
        let rows =
            query_map_cells(&backend, "SELECT id, body FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.cell_count(), 2);
        }
    }

    #[test]
    fn query_map_cells_empty_table_returns_empty_vec() {
        let backend = fresh_rusqlite();
        backend.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        let rows = query_map_cells(&backend, "SELECT x FROM t", &[]).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn query_row_cells_propagates_backend_error() {
        let backend = fresh_rusqlite();
        let err = query_row_cells(&backend, "SELECT * FROM does_not_exist", &[])
            .unwrap_err();
        // The substrate's BackendError is opaque here; we just
        // assert the error path exists rather than match a specific
        // discriminant the substrate may evolve.
        let _ = format!("{err}");
    }
}
