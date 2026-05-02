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
    u32::try_from(v).map_err(|_| {
        BackendError::Query(format!("row column {idx}: u32 out of range: {v}"))
    })
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
/// NULL encoding to `None`. Note: this is lossy when the
/// underlying value is a real empty TEXT — the wired-pass
/// `Row` accessor abstraction (cont-bead scope item 3) carries
/// the type tag and will distinguish, but the string-substrate
/// flattens both to `None` here.
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
        assert_eq!(
            row_optional_string(&r, 1).unwrap(),
            Some("set".to_string())
        );
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
}
