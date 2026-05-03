//! Typed row-mapper helpers over [`StorageBackend`] string columns.
//!
//! **Bead:** br-ft-l1jgo (wa-2l27x.8.cont.extract.callsite)
//! **Substrate:** ft-qgj81 — ships
//! [`StorageBackend::query_row_strings`] +
//! [`StorageBackend::query_map_strings`] returning columns
//! encoded per [`encode_sqlite_value_as_string`] canonical rules.
//!
//! ## Why this module
//!
//! `query_row_strings` returns each column as a `String` so the
//! trait stays object-safe + cheap to ship. The wired-pass
//! call-site migration in storage.rs needs typed extraction
//! (`i64`, `Option<String>`, blob-size, NULL-as-default) on
//! every row. This module ships the typed combinators every
//! call site uses + a [`RowReader`] view that bundles them
//! against a single row.
//!
//! Each helper mirrors a column-encoding rule from
//! [`encode_sqlite_value_as_string`]:
//!
//! - `INTEGER` → [`row_i64`] / [`row_u32`].
//! - `REAL`    → [`row_f64`].
//! - `TEXT`    → [`row_string`] / [`row_optional_string`].
//! - `BLOB`    → [`row_blob_size`] (parses `<blob:N bytes>` → N).
//! - `NULL`    → empty string; the optional helpers return
//!               `None`.
//!
//! The module is wholly contained — touching it does not modify
//! storage.rs. The wired-pass call-site migration imports from
//! here on a per-cluster basis.

use crate::storage_backend_trait::BackendError;

/// Parse a column as `i64`. NULL columns (empty string) and
/// non-decimal text return [`BackendError::Query`].
pub fn row_i64(row: &[String], idx: usize) -> Result<i64, BackendError> {
    let cell = row_cell(row, idx)?;
    if cell.is_empty() {
        return Err(BackendError::Query(format!(
            "row column {idx}: expected i64, got NULL / empty string"
        )));
    }
    cell.parse::<i64>().map_err(|err| {
        BackendError::Query(format!(
            "row column {idx}: i64 parse failed for `{cell}`: {err}"
        ))
    })
}

/// Parse a column as `i64`, returning the supplied default when
/// the column is NULL (empty string).
pub fn row_i64_or(row: &[String], idx: usize, default: i64) -> Result<i64, BackendError> {
    let cell = row_cell(row, idx)?;
    if cell.is_empty() {
        return Ok(default);
    }
    cell.parse::<i64>().map_err(|err| {
        BackendError::Query(format!(
            "row column {idx}: i64 parse failed for `{cell}`: {err}"
        ))
    })
}

/// Parse a column as `u32`. NULL or negative values fail.
pub fn row_u32(row: &[String], idx: usize) -> Result<u32, BackendError> {
    let v = row_i64(row, idx)?;
    u32::try_from(v)
        .map_err(|_| BackendError::Query(format!("row column {idx}: u32 out of range: {v}")))
}

/// Parse a column as `f64`. NULL columns fail.
pub fn row_f64(row: &[String], idx: usize) -> Result<f64, BackendError> {
    let cell = row_cell(row, idx)?;
    if cell.is_empty() {
        return Err(BackendError::Query(format!(
            "row column {idx}: expected f64, got NULL / empty string"
        )));
    }
    cell.parse::<f64>().map_err(|err| {
        BackendError::Query(format!(
            "row column {idx}: f64 parse failed for `{cell}`: {err}"
        ))
    })
}

/// Read a column as a `String`, taking ownership.
///
/// Empty strings (which encode NULL per the substrate's column
/// encoding) round-trip as empty strings — callers that want to
/// distinguish NULL from real-empty-text should use
/// [`row_optional_string`] instead, which only the wired-pass
/// can implement (the substrate's string round-trip is lossy
/// for the NULL-vs-empty distinction).
pub fn row_string(row: &[String], idx: usize) -> Result<String, BackendError> {
    Ok(row_cell(row, idx)?.to_string())
}

/// Read a column as `Option<String>`, mapping the empty-string
/// NULL encoding to `None`.
///
/// **Lossy.** The substrate's string-encoded row pipeline
/// (`query_row_strings` / `query_map_strings`) flattens both
/// SQL NULL and a real empty TEXT to `None` here. Most call
/// sites genuinely don't care (account names, descriptions,
/// reasons — an empty value is operationally indistinguishable
/// from "never set"). Call sites that DO need to distinguish
/// `Some("")` from `None` must avoid the string-substrate
/// entirely and use the `SqlCell`-based pipeline:
/// [`StorageBackend::query_row_cells`] /
/// [`StorageBackend::query_map_cells`] feeding
/// [`cell_optional_string`] / [`CellRowReader::optional_string`].
pub fn row_optional_string(row: &[String], idx: usize) -> Result<Option<String>, BackendError> {
    let cell = row_cell(row, idx)?;
    if cell.is_empty() {
        Ok(None)
    } else {
        Ok(Some(cell.to_string()))
    }
}

/// Parse a column as `bool`. Accepts SQLite's INTEGER 0/1
/// encoding (post-decimal-string formatting) + the lowercase
/// "true" / "false" text encoding. Anything else is a parse
/// error.
pub fn row_bool(row: &[String], idx: usize) -> Result<bool, BackendError> {
    let cell = row_cell(row, idx)?;
    match cell {
        "0" | "false" => Ok(false),
        "1" | "true" => Ok(true),
        other => Err(BackendError::Query(format!(
            "row column {idx}: bool parse failed for `{other}` \
             (expected 0/1/true/false)"
        ))),
    }
}

/// Parse a column carrying a `<blob:N bytes>` encoding (the
/// substrate's BLOB column placeholder) and return `N`. Used by
/// diagnostic queries that report blob sizes in
/// `database_page_stats` / similar.
pub fn row_blob_size(row: &[String], idx: usize) -> Result<usize, BackendError> {
    let cell = row_cell(row, idx)?;
    let stripped = cell
        .strip_prefix("<blob:")
        .and_then(|s| s.strip_suffix(" bytes>"))
        .ok_or_else(|| {
            BackendError::Query(format!(
                "row column {idx}: expected `<blob:N bytes>`, got `{cell}`"
            ))
        })?;
    stripped.parse::<usize>().map_err(|err| {
        BackendError::Query(format!(
            "row column {idx}: blob size parse failed for `{stripped}`: {err}"
        ))
    })
}

/// Read the column at `idx`, returning a [`BackendError::Query`]
/// when out of range. All typed helpers route through this.
fn row_cell(row: &[String], idx: usize) -> Result<&str, BackendError> {
    row.get(idx).map(String::as_str).ok_or_else(|| {
        BackendError::Query(format!(
            "row column {idx}: out of range (row has {} columns)",
            row.len()
        ))
    })
}

/// Borrow-style row reader bundling the typed extractors against
/// a single row. Lets call sites read multiple columns from the
/// same row without repeating the row reference.
///
/// ```ignore
/// let row = backend.query_row_strings("SELECT id, name, age FROM p", &[])?
///     .ok_or_else(|| BackendError::Query("no row".into()))?;
/// let r = RowReader::new(&row);
/// let id: i64 = r.i64(0)?;
/// let name: String = r.string(1)?;
/// let age: Option<i64> = r.optional_i64(2)?;
/// ```
pub struct RowReader<'a> {
    row: &'a [String],
}

impl<'a> RowReader<'a> {
    /// Wrap a row for typed extraction.
    #[must_use]
    pub fn new(row: &'a [String]) -> Self {
        Self { row }
    }

    /// Underlying column count.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.row.len()
    }

    /// See [`row_i64`].
    pub fn i64(&self, idx: usize) -> Result<i64, BackendError> {
        row_i64(self.row, idx)
    }

    /// See [`row_i64_or`].
    pub fn i64_or(&self, idx: usize, default: i64) -> Result<i64, BackendError> {
        row_i64_or(self.row, idx, default)
    }

    /// See [`row_u32`].
    pub fn u32(&self, idx: usize) -> Result<u32, BackendError> {
        row_u32(self.row, idx)
    }

    /// See [`row_f64`].
    pub fn f64(&self, idx: usize) -> Result<f64, BackendError> {
        row_f64(self.row, idx)
    }

    /// See [`row_string`].
    pub fn string(&self, idx: usize) -> Result<String, BackendError> {
        row_string(self.row, idx)
    }

    /// See [`row_optional_string`].
    pub fn optional_string(&self, idx: usize) -> Result<Option<String>, BackendError> {
        row_optional_string(self.row, idx)
    }

    /// Optional `i64`: NULL / empty → None.
    pub fn optional_i64(&self, idx: usize) -> Result<Option<i64>, BackendError> {
        let cell = row_cell(self.row, idx)?;
        if cell.is_empty() {
            return Ok(None);
        }
        cell.parse::<i64>().map(Some).map_err(|err| {
            BackendError::Query(format!(
                "row column {idx}: optional i64 parse failed for `{cell}`: {err}"
            ))
        })
    }

    /// See [`row_bool`].
    pub fn bool(&self, idx: usize) -> Result<bool, BackendError> {
        row_bool(self.row, idx)
    }

    /// See [`row_blob_size`].
    pub fn blob_size(&self, idx: usize) -> Result<usize, BackendError> {
        row_blob_size(self.row, idx)
    }
}

// ============================================================================
// SqlCell-based row helpers (NULL vs empty-TEXT preserving).
//
// These mirror the `row_*` family above but consume `&[SqlCell]` rows from
// `StorageBackend::query_row_cells` / `query_map_cells`. They preserve every
// distinction the string-encoded pipeline collapses, most importantly
// `SqlCell::Null` vs `SqlCell::Text(String::new())` for nullable TEXT
// columns where the difference is operationally meaningful (e.g. user-set
// empty bookmark description vs. never-set).
//
// Two-pipeline coexistence is intentional. The string pipeline is fine for
// most call sites and stays the default; new call sites that need the
// stricter semantics opt into the cell pipeline.
// ============================================================================

use crate::storage_backend_trait::SqlCell;

/// Read the cell at `idx` from a `&[SqlCell]` row, returning a
/// [`BackendError::Query`] when out of range.
fn cell_at(row: &[SqlCell], idx: usize) -> Result<&SqlCell, BackendError> {
    row.get(idx).ok_or_else(|| {
        BackendError::Query(format!(
            "row column {idx}: out of range (row has {} cells)",
            row.len()
        ))
    })
}

/// Read a column as `String`. NULL fails loudly so callers can't
/// silently coerce a missing value to an empty one. Empty TEXT
/// returns `String::new()` (preserved exactly).
pub fn cell_string(row: &[SqlCell], idx: usize) -> Result<String, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Text(s) => Ok(s.clone()),
        SqlCell::Null => Err(BackendError::Query(format!(
            "row column {idx}: expected TEXT, got NULL"
        ))),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected TEXT, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Read a column as `Option<String>`, preserving the NULL vs
/// empty-TEXT distinction.
///
/// - `SqlCell::Null` → `None`
/// - `SqlCell::Text(s)` → `Some(s)` (including `Some(String::new())`)
/// - any other variant → [`BackendError::Query`]
pub fn cell_optional_string(
    row: &[SqlCell],
    idx: usize,
) -> Result<Option<String>, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Null => Ok(None),
        SqlCell::Text(s) => Ok(Some(s.clone())),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected TEXT or NULL, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Read a column as `i64`. NULL and non-Integer cells fail.
pub fn cell_i64(row: &[SqlCell], idx: usize) -> Result<i64, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Integer(i) => Ok(*i),
        SqlCell::Null => Err(BackendError::Query(format!(
            "row column {idx}: expected INTEGER, got NULL"
        ))),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected INTEGER, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Read a column as `Option<i64>`. NULL → None.
pub fn cell_optional_i64(
    row: &[SqlCell],
    idx: usize,
) -> Result<Option<i64>, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Null => Ok(None),
        SqlCell::Integer(i) => Ok(Some(*i)),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected INTEGER or NULL, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Read a column as `f64`. NULL and non-Real/Integer cells fail.
/// Integer cells are promoted to `f64` losslessly for values
/// representable as `f64`.
pub fn cell_f64(row: &[SqlCell], idx: usize) -> Result<f64, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Real(f) => Ok(*f),
        #[allow(clippy::cast_precision_loss)]
        SqlCell::Integer(i) => Ok(*i as f64),
        SqlCell::Null => Err(BackendError::Query(format!(
            "row column {idx}: expected REAL, got NULL"
        ))),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected REAL, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Read a column as `bool`. SQLite has no bool — accepts the
/// INTEGER-encoded 0/1 form (the only form Frankenterm writes).
pub fn cell_bool(row: &[SqlCell], idx: usize) -> Result<bool, BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Integer(0) => Ok(false),
        SqlCell::Integer(1) => Ok(true),
        SqlCell::Integer(other) => Err(BackendError::Query(format!(
            "row column {idx}: expected bool 0/1, got {other}"
        ))),
        SqlCell::Null => Err(BackendError::Query(format!(
            "row column {idx}: expected bool 0/1, got NULL"
        ))),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected bool 0/1, got {}",
            cell_kind_label(other)
        ))),
    }
}

/// Borrow a column as `&[u8]` when the cell is a Blob.
pub fn cell_blob<'a>(row: &'a [SqlCell], idx: usize) -> Result<&'a [u8], BackendError> {
    match cell_at(row, idx)? {
        SqlCell::Blob(b) => Ok(b.as_slice()),
        SqlCell::Null => Err(BackendError::Query(format!(
            "row column {idx}: expected BLOB, got NULL"
        ))),
        other => Err(BackendError::Query(format!(
            "row column {idx}: expected BLOB, got {}",
            cell_kind_label(other)
        ))),
    }
}

const fn cell_kind_label(cell: &SqlCell) -> &'static str {
    match cell {
        SqlCell::Null => "NULL",
        SqlCell::Integer(_) => "INTEGER",
        SqlCell::Real(_) => "REAL",
        SqlCell::Text(_) => "TEXT",
        SqlCell::Blob(_) => "BLOB",
    }
}

/// Borrow-style row reader bundling the SqlCell-based extractors
/// against a single row from `query_row_cells` / `query_map_cells`.
/// Mirrors [`RowReader`] but preserves NULL vs empty-TEXT for
/// callers that need the distinction.
pub struct CellRowReader<'a> {
    row: &'a [SqlCell],
}

impl<'a> CellRowReader<'a> {
    /// Wrap a row for typed extraction.
    #[must_use]
    pub fn new(row: &'a [SqlCell]) -> Self {
        Self { row }
    }

    /// Underlying column count.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.row.len()
    }

    /// Whether the cell at `idx` is SQL NULL. Out-of-range is treated
    /// as "not NULL" — the typed accessors will surface the index
    /// error when the caller actually reads the column.
    #[must_use]
    pub fn is_null(&self, idx: usize) -> bool {
        self.row.get(idx).is_some_and(SqlCell::is_null)
    }

    /// See [`cell_string`].
    pub fn string(&self, idx: usize) -> Result<String, BackendError> {
        cell_string(self.row, idx)
    }

    /// See [`cell_optional_string`].
    pub fn optional_string(&self, idx: usize) -> Result<Option<String>, BackendError> {
        cell_optional_string(self.row, idx)
    }

    /// See [`cell_i64`].
    pub fn i64(&self, idx: usize) -> Result<i64, BackendError> {
        cell_i64(self.row, idx)
    }

    /// See [`cell_optional_i64`].
    pub fn optional_i64(&self, idx: usize) -> Result<Option<i64>, BackendError> {
        cell_optional_i64(self.row, idx)
    }

    /// See [`cell_f64`].
    pub fn f64(&self, idx: usize) -> Result<f64, BackendError> {
        cell_f64(self.row, idx)
    }

    /// See [`cell_bool`].
    pub fn bool(&self, idx: usize) -> Result<bool, BackendError> {
        cell_bool(self.row, idx)
    }

    /// See [`cell_blob`].
    pub fn blob(&self, idx: usize) -> Result<&'a [u8], BackendError> {
        cell_blob(self.row, idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn row_i64_parses_positive_negative_zero() {
        let r = row(&["42", "-7", "0"]);
        assert_eq!(row_i64(&r, 0).unwrap(), 42);
        assert_eq!(row_i64(&r, 1).unwrap(), -7);
        assert_eq!(row_i64(&r, 2).unwrap(), 0);
    }

    #[test]
    fn row_i64_rejects_null_and_garbage() {
        let r = row(&["", "abc"]);
        assert!(matches!(row_i64(&r, 0), Err(BackendError::Query(_))));
        assert!(matches!(row_i64(&r, 1), Err(BackendError::Query(_))));
    }

    #[test]
    fn row_i64_or_returns_default_for_null() {
        let r = row(&["", "5"]);
        assert_eq!(row_i64_or(&r, 0, 99).unwrap(), 99);
        assert_eq!(row_i64_or(&r, 1, 99).unwrap(), 5);
    }

    #[test]
    fn row_u32_rejects_negative() {
        let r = row(&["-1"]);
        assert!(matches!(row_u32(&r, 0), Err(BackendError::Query(_))));
    }

    #[test]
    fn row_f64_parses_decimal_strings() {
        let r = row(&["1.5", "0.0", "-3.14"]);
        assert!((row_f64(&r, 0).unwrap() - 1.5).abs() < f64::EPSILON);
        assert!((row_f64(&r, 1).unwrap() - 0.0).abs() < f64::EPSILON);
        assert!((row_f64(&r, 2).unwrap() - -3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn row_string_round_trips() {
        let r = row(&["hello", "world"]);
        assert_eq!(row_string(&r, 0).unwrap(), "hello");
        assert_eq!(row_string(&r, 1).unwrap(), "world");
    }

    #[test]
    fn row_optional_string_maps_empty_to_none() {
        let r = row(&["", "set"]);
        assert_eq!(row_optional_string(&r, 0).unwrap(), None);
        assert_eq!(row_optional_string(&r, 1).unwrap(), Some("set".to_string()));
    }

    #[test]
    fn row_bool_accepts_sqlite_int_and_text_forms() {
        let r = row(&["0", "1", "true", "false"]);
        assert!(!row_bool(&r, 0).unwrap());
        assert!(row_bool(&r, 1).unwrap());
        assert!(row_bool(&r, 2).unwrap());
        assert!(!row_bool(&r, 3).unwrap());
    }

    #[test]
    fn row_bool_rejects_unknown() {
        let r = row(&["yes", "2"]);
        assert!(matches!(row_bool(&r, 0), Err(BackendError::Query(_))));
        assert!(matches!(row_bool(&r, 1), Err(BackendError::Query(_))));
    }

    #[test]
    fn row_blob_size_parses_substrate_placeholder() {
        let r = row(&["<blob:0 bytes>", "<blob:1024 bytes>", "<blob:65536 bytes>"]);
        assert_eq!(row_blob_size(&r, 0).unwrap(), 0);
        assert_eq!(row_blob_size(&r, 1).unwrap(), 1024);
        assert_eq!(row_blob_size(&r, 2).unwrap(), 65536);
    }

    #[test]
    fn row_blob_size_rejects_non_blob_cell() {
        let r = row(&["1024", "blob:1024", "<blob:1024>"]);
        assert!(matches!(row_blob_size(&r, 0), Err(BackendError::Query(_))));
        assert!(matches!(row_blob_size(&r, 1), Err(BackendError::Query(_))));
        assert!(matches!(row_blob_size(&r, 2), Err(BackendError::Query(_))));
    }

    #[test]
    fn out_of_range_idx_is_query_error() {
        let r = row(&["a"]);
        assert!(matches!(row_i64(&r, 5), Err(BackendError::Query(_))));
        assert!(matches!(row_string(&r, 5), Err(BackendError::Query(_))));
        assert!(matches!(row_blob_size(&r, 5), Err(BackendError::Query(_))));
    }

    #[test]
    fn row_reader_bundles_extractors_against_one_row() {
        let r = row(&["42", "alpha", "1.5", "1", "<blob:128 bytes>", ""]);
        let reader = RowReader::new(&r);
        assert_eq!(reader.column_count(), 6);
        assert_eq!(reader.i64(0).unwrap(), 42);
        assert_eq!(reader.string(1).unwrap(), "alpha");
        assert!((reader.f64(2).unwrap() - 1.5).abs() < f64::EPSILON);
        assert!(reader.bool(3).unwrap());
        assert_eq!(reader.blob_size(4).unwrap(), 128);
        assert_eq!(reader.optional_string(5).unwrap(), None);
    }

    #[test]
    fn row_reader_optional_i64_maps_empty_to_none() {
        let r = row(&["", "100"]);
        let reader = RowReader::new(&r);
        assert_eq!(reader.optional_i64(0).unwrap(), None);
        assert_eq!(reader.optional_i64(1).unwrap(), Some(100));
    }

    // ── string-pipeline lossy semantics: pinned ──────────────────
    //
    // The string-encoded row pipeline cannot distinguish SQL NULL
    // from empty TEXT. This test pins that behavior so anyone
    // migrating a column where the distinction matters has to
    // change the test, signalling they should switch to the
    // SqlCell-based pipeline below.
    #[test]
    fn row_optional_string_collapses_null_and_empty_text() {
        let r = row(&[""]);
        // Both "this column is NULL in the DB" and "this column is
        // a real empty TEXT in the DB" arrive here as empty
        // strings — we have no way to tell them apart.
        assert_eq!(row_optional_string(&r, 0).unwrap(), None);
    }

    // ── SqlCell-pipeline strict semantics ────────────────────────

    #[test]
    fn cell_optional_string_distinguishes_null_from_empty_text() {
        // SQL NULL → None.
        let row_null = vec![SqlCell::Null];
        assert_eq!(cell_optional_string(&row_null, 0).unwrap(), None);

        // TEXT("") → Some(""). The whole point of the strict path.
        let row_empty = vec![SqlCell::Text(String::new())];
        assert_eq!(
            cell_optional_string(&row_empty, 0).unwrap(),
            Some(String::new())
        );

        // TEXT("x") → Some("x").
        let row_value = vec![SqlCell::Text("x".to_string())];
        assert_eq!(
            cell_optional_string(&row_value, 0).unwrap(),
            Some("x".to_string())
        );

        // INTEGER in a TEXT column is a query authoring error;
        // surface it loudly rather than silently coercing.
        let row_wrong = vec![SqlCell::Integer(0)];
        assert!(cell_optional_string(&row_wrong, 0).is_err());
    }

    #[test]
    fn cell_string_rejects_null_and_preserves_empty_text() {
        let row_null = vec![SqlCell::Null];
        assert!(cell_string(&row_null, 0).is_err());

        let row_empty = vec![SqlCell::Text(String::new())];
        assert_eq!(cell_string(&row_empty, 0).unwrap(), String::new());

        let row_value = vec![SqlCell::Text("hello".to_string())];
        assert_eq!(cell_string(&row_value, 0).unwrap(), "hello");
    }

    #[test]
    fn cell_typed_helpers_reject_type_mismatches() {
        let row = vec![
            SqlCell::Integer(42),
            SqlCell::Real(3.14),
            SqlCell::Text("abc".to_string()),
            SqlCell::Null,
            SqlCell::Blob(vec![0xde, 0xad]),
        ];

        assert_eq!(cell_i64(&row, 0).unwrap(), 42);
        assert!(cell_i64(&row, 2).is_err()); // text is not int
        assert!(cell_i64(&row, 3).is_err()); // null is not int

        assert!((cell_f64(&row, 1).unwrap() - 3.14).abs() < f64::EPSILON);
        assert!((cell_f64(&row, 0).unwrap() - 42.0).abs() < f64::EPSILON); // int promotes
        assert!(cell_f64(&row, 3).is_err());

        assert_eq!(cell_optional_i64(&row, 3).unwrap(), None);
        assert_eq!(cell_optional_i64(&row, 0).unwrap(), Some(42));

        assert_eq!(cell_blob(&row, 4).unwrap(), &[0xde, 0xad]);
        assert!(cell_blob(&row, 0).is_err());
    }

    #[test]
    fn cell_bool_accepts_only_canonical_zero_one() {
        let zero = vec![SqlCell::Integer(0)];
        let one = vec![SqlCell::Integer(1)];
        let two = vec![SqlCell::Integer(2)];
        let txt = vec![SqlCell::Text("true".to_string())];

        assert!(!cell_bool(&zero, 0).unwrap());
        assert!(cell_bool(&one, 0).unwrap());
        assert!(cell_bool(&two, 0).is_err());
        assert!(cell_bool(&txt, 0).is_err());
    }

    #[test]
    fn cell_row_reader_bundles_strict_extractors() {
        let row = vec![
            SqlCell::Integer(7),
            SqlCell::Text(String::new()),
            SqlCell::Null,
            SqlCell::Real(2.5),
            SqlCell::Integer(1),
        ];
        let reader = CellRowReader::new(&row);
        assert_eq!(reader.column_count(), 5);
        assert!(!reader.is_null(0));
        assert!(reader.is_null(2));
        assert!(!reader.is_null(99)); // out-of-range is not NULL

        assert_eq!(reader.i64(0).unwrap(), 7);
        // Empty TEXT survives the round-trip — the bug HIGH-5 was
        // about avoiding precisely this collapse to None.
        assert_eq!(reader.optional_string(1).unwrap(), Some(String::new()));
        assert_eq!(reader.optional_string(2).unwrap(), None);
        assert!((reader.f64(3).unwrap() - 2.5).abs() < f64::EPSILON);
        assert!(reader.bool(4).unwrap());
    }

    #[test]
    fn cell_helpers_surface_out_of_range_consistently() {
        let row: Vec<SqlCell> = vec![SqlCell::Integer(1)];
        assert!(matches!(cell_i64(&row, 5), Err(BackendError::Query(_))));
        assert!(matches!(
            cell_optional_string(&row, 5),
            Err(BackendError::Query(_))
        ));
    }
}
