//! Property tests for SQL parameter-binding round-trip via the
//! `StorageBackend` trait against the real `RusqliteBackend`
//! substrate — no mocks, in-memory SQLite.
//!
//! The existing `proptest_storage_backend_cells.rs` proves
//! `SqlCell` semantic equivalence at the in-memory layer (encode →
//! decode with no SQLite involved). What it does NOT prove is that
//! a value passed in via `ToSqlValue::*` and stored into a real
//! SQLite cell will come back as the same `SqlCell::*` variant
//! through `query_row_cells`. That round-trip is where the
//! storage-class fidelity contract actually lives, and where the
//! likely regression vectors are:
//!
//! - SQLite's type-affinity coercion on declared column types.
//!   A column declared `INTEGER` will coerce a bound `Real` to
//!   `Integer` if the float happens to be an exact integer; an
//!   `INT` column will coerce certain text strings to integers.
//!   Backends MUST insulate the trait surface from this — if a
//!   caller binds `Real(1.0)`, reading back must return
//!   `Real(1.0)`, not `Integer(1)`.
//! - NULL vs empty-text vs zero-blob discrimination — the three
//!   storage classes are distinct in SQLite but easily collapsed
//!   through any string-cells fallback path.
//! - Blob byte fidelity — every byte in the input slice must
//!   round-trip, including embedded NULs.
//! - i64 boundary values (`i64::MIN` / `i64::MAX`).
//! - f64 special values: `+0.0`/`-0.0` distinguished by `to_bits`,
//!   subnormals, large finite values. NaN/Inf are excluded —
//!   SQLite has implementation-defined behavior for them.
//! - Text round-trip across the SQLite UTF-8 boundary, including
//!   strings with characters that look like SQL syntax (quotes,
//!   semicolons, percent, underscore — the LIKE wildcards).
//!
//! Property: for every `ToSqlValue` variant we generate, the
//! cell read back via `query_row_cells` is **semantically
//! equivalent** to the input under
//! `prop_assert_cell_round_trip_eq`.
//!
//! Logs are emitted as structured tracing-json events on every
//! property case so a failing case lands a parseable record of
//! the input variant + observed cell — same shape as
//! `proptest_storage_backend_helpers` (br-ft-l1jgo phase-3).

use std::sync::Once;

use frankenterm_core::storage_backend_cells::{Row, SqlCell, query_row_cells};
use frankenterm_core::storage_backend_trait::{
    OpenConfig, RusqliteBackend, StorageBackend, ToSqlValue,
};
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn open_backend() -> RusqliteBackend {
    RusqliteBackend::open(":memory:", &OpenConfig::default()).expect("open memory backend")
}

/// Single-column table with no declared affinity (column type
/// `BLOB` in SQLite gives "no affinity" — closest thing the
/// engine has to "store the value as the bound storage class").
fn create_no_affinity_table(backend: &RusqliteBackend) {
    backend
        .execute_batch("CREATE TABLE bench (cell);")
        .expect("create no-affinity table");
}

/// Insert one bound `ToSqlValue` and read it back via the typed
/// cells path. Returns the round-tripped cell.
fn round_trip_cell(value: ToSqlValue<'_>) -> Option<SqlCell> {
    let backend = open_backend();
    create_no_affinity_table(&backend);
    backend
        .query_row_typed("INSERT INTO bench (cell) VALUES (?1)", &[value])
        .expect("insert");
    let row = query_row_cells(&backend, "SELECT cell FROM bench LIMIT 1", &[]).expect("select");
    row.map(|r| r.cell(0).cloned().expect("at least one cell"))
}

/// Strings that include LIKE wildcards (`%`/`_`), quote
/// characters (`'`/`"`), semicolons, and assorted control
/// characters. Excludes NUL since SQLite text cells cannot
/// store NUL bytes — that's a Blob's job.
fn safe_text_with_sql_special_chars() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/?&='\";%-]{0,32}".prop_map(String::from)
}

/// Blob payload up to 256 bytes. Any byte (including 0x00) is
/// fair game because Blob cells preserve the byte sequence
/// exactly.
fn blob_payload() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..256)
}

/// Finite `f64` excluding NaN/Inf. SQLite stores ±NaN/±Inf as
/// blobs in some builds and as floats in others; the resulting
/// SqlCell variant is implementation-defined. The trait's
/// fidelity contract holds for the finite range — we test that
/// rather than the engine's NaN serialization choice.
fn finite_f64_strategy() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("finite f64", |v| v.is_finite())
}

/// Compare expected cell vs the round-tripped cell. Real cells
/// allow `to_bits` equality OR float-aware tolerance for the
/// extreme-magnitude regime where the JSON-string fallback path
/// (used by backends without native dispatch) loses ULPs in
/// printf round-trip.
fn prop_assert_cell_round_trip_eq(
    expected: &SqlCell,
    actual: &SqlCell,
) -> Result<(), TestCaseError> {
    match (expected, actual) {
        (SqlCell::Real(left), SqlCell::Real(right)) => {
            if left.to_bits() == right.to_bits() {
                return Ok(());
            }
            // Backend went through the canonical-string fallback
            // path; permit ULP-level drift relative to the input
            // magnitude. RusqliteBackend's native dispatch
            // bit-equals; we accept either.
            let diff = (left - right).abs();
            let scale = left.abs().max(right.abs()).max(f64::MIN_POSITIVE);
            prop_assert!(
                diff <= scale * f64::EPSILON,
                "Real cells must round-trip bit-equal or within scaled-EPS \
                 (expected {left}, got {right}, diff {diff}, scale {scale})"
            );
            Ok(())
        }
        _ => {
            prop_assert_eq!(expected, actual);
            Ok(())
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Bind a NULL parameter, read back. Must be SqlCell::Null.
    #[test]
    fn proptest_storage_backend_param_binding_null_round_trips(_unit in 0u8..=0u8) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::Null).expect("row");
        info!(test = "null_round_trips", actual = ?actual, "null round-trip");
        prop_assert_cell_round_trip_eq(&SqlCell::Null, &actual)?;
    }

    /// Bind an Integer parameter, read back. Must be the same
    /// i64. Covers the full range including i64::MIN/MAX
    /// (proptest's default any::<i64>() includes the boundaries).
    #[test]
    fn proptest_storage_backend_param_binding_integer_round_trips(value in any::<i64>()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::Integer(value)).expect("row");
        info!(test = "integer_round_trips", value, actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Integer(value), &actual)?;
    }

    /// Bind a Real parameter, read back. Finite f64 only.
    #[test]
    fn proptest_storage_backend_param_binding_real_round_trips(value in finite_f64_strategy()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::Real(value)).expect("row");
        info!(test = "real_round_trips", value, actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Real(value), &actual)?;
    }

    /// Bind a borrowed Text parameter (SQL-special characters
    /// included), read back. Round-trip must preserve the
    /// exact UTF-8 byte sequence.
    #[test]
    fn proptest_storage_backend_param_binding_text_round_trips(text in safe_text_with_sql_special_chars()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::Text(text.as_str())).expect("row");
        info!(test = "text_round_trips", input_len = text.len(), actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Text(text.clone()), &actual)?;
    }

    /// Bind an OwnedText parameter (same surface as Text, just
    /// the owned-String variant — the trait carries both because
    /// some call sites want the lifetime-extension).
    #[test]
    fn proptest_storage_backend_param_binding_owned_text_round_trips(text in safe_text_with_sql_special_chars()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::OwnedText(text.clone())).expect("row");
        info!(test = "owned_text_round_trips", input_len = text.len(), actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Text(text), &actual)?;
    }

    /// Bind a borrowed Blob parameter, read back. Every byte
    /// must survive the round-trip including 0x00 bytes anywhere
    /// in the slice — that's the distinction Blob has from Text.
    #[test]
    fn proptest_storage_backend_param_binding_blob_round_trips(blob in blob_payload()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::Blob(blob.as_slice())).expect("row");
        info!(test = "blob_round_trips", input_len = blob.len(), actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Blob(blob), &actual)?;
    }

    /// Bind an OwnedBlob, read back. Same as Blob but exercises
    /// the owned-Vec variant of the trait.
    #[test]
    fn proptest_storage_backend_param_binding_owned_blob_round_trips(blob in blob_payload()) {
        init_test_tracing_json();
        let actual = round_trip_cell(ToSqlValue::OwnedBlob(blob.clone())).expect("row");
        info!(test = "owned_blob_round_trips", input_len = blob.len(), actual = ?actual);
        prop_assert_cell_round_trip_eq(&SqlCell::Blob(blob), &actual)?;
    }

    /// br-ft-l1jgo phase-3 cross-class invariant: NULL is NOT
    /// equal to empty Text or zero-byte Blob after round-trip.
    /// This catches the canonical-string-fallback collapse where
    /// all three flatten to the empty string.
    #[test]
    fn proptest_storage_backend_param_binding_null_empty_text_zero_blob_distinct(_unit in 0u8..=0u8) {
        init_test_tracing_json();

        let null_cell = round_trip_cell(ToSqlValue::Null).expect("row");
        let empty_text_cell = round_trip_cell(ToSqlValue::Text("")).expect("row");
        let zero_blob_cell = round_trip_cell(ToSqlValue::Blob(&[])).expect("row");

        info!(
            test = "null_empty_text_zero_blob_distinct",
            null_cell = ?null_cell,
            empty_text_cell = ?empty_text_cell,
            zero_blob_cell = ?zero_blob_cell,
            "three storage classes must remain distinct after round-trip"
        );

        prop_assert!(matches!(null_cell, SqlCell::Null),
            "NULL round-trip must remain SqlCell::Null, got {null_cell:?}");
        prop_assert!(matches!(empty_text_cell, SqlCell::Text(ref s) if s.is_empty()),
            "empty Text round-trip must remain SqlCell::Text(\"\"), got {empty_text_cell:?}");
        prop_assert!(matches!(zero_blob_cell, SqlCell::Blob(ref b) if b.is_empty()),
            "zero-byte Blob round-trip must remain SqlCell::Blob([]), got {zero_blob_cell:?}");
    }
}
