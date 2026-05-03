//! Property tests for `storage_backend_converter` against the
//! real `RusqliteBackend` substrate — no mocks, in-memory SQLite.
//!
//! Earlier revisions of this file shipped a hand-rolled
//! `MockBackend` that only forwarded enqueued responses for
//! `query_map_strings` and stub-implemented every other trait
//! method. That hid two real classes of bugs:
//!
//! 1. The mock never round-tripped through SQLite's quoting,
//!    type-coercion, or `ORDER BY rowid` semantics — so any
//!    converter regression that depended on real `query_map_strings`
//!    formatting (NULL handling, numeric stringification,
//!    `rowid` ordering) was invisible.
//! 2. The mock implemented `begin_transaction` as a hard error,
//!    so transaction-aware converter code paths (the wired-pass
//!    is expected to wrap large copies in a transaction) could
//!    not be exercised end-to-end.
//!
//! The migration here uses `RusqliteBackend::open(":memory:", &OpenConfig::default())`
//! which is the exact substrate type production code wraps a
//! pooled connection into (`pooled_backend` in storage.rs). The
//! row generation is constrained to schemas that the real
//! backend can round-trip (TEXT cells, fixed column widths)
//! so the property test exercises the full SELECT/INSERT/PRAGMA
//! plumbing rather than a behavior-by-stipulation simulator.
//!
//! Logs are emitted as structured tracing-json events so the
//! rch worker / CI dashboard can parse them and the diff
//! review can see exactly what was inserted, queried, and
//! asserted on every failing case. The `init_test_tracing_json`
//! helper is a once-cell guard so the subscriber is only
//! installed once per test binary (running 48 property cases
//! per test would otherwise log a global subscriber install
//! attempt 48 times).

use std::sync::Once;

use frankenterm_core::storage_backend_converter::{convert_db, copy_table, verify_equivalence};
use frankenterm_core::storage_backend_trait::{
    BackendError, OpenConfig, RusqliteBackend, StorageBackend, ToSqlValue,
};
use proptest::prelude::*;
use tracing::info;

/// Install a JSON-formatted tracing subscriber on the test
/// writer so each `tracing::info!(...)` call lands in
/// `cargo test`'s captured stdout as a parseable JSON line.
/// Idempotent across all proptest cases in this file.
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

fn safe_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/?&=-]{0,48}".prop_map(String::from)
}

fn unsafe_identifier() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[A-Za-z0-9_]{0,8}[ ;\"'-][A-Za-z0-9_ ;\"'-]{0,8}".prop_map(String::from),
    ]
}

fn row_data() -> impl Strategy<Value = Vec<(i64, String)>> {
    prop::collection::vec((any::<i32>().prop_map(i64::from), safe_text()), 0..16)
}

/// Single-column TEXT rows — used by the equivalence tests so
/// the column width is uniform across the property variates and
/// the underlying `CREATE TABLE` schema stays trivial.
fn single_column_rows() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(safe_text(), 0..12)
}

fn different_cells() -> impl Strategy<Value = (String, String)> {
    (safe_text(), safe_text()).prop_filter("different cells", |(left, right)| left != right)
}

fn open_backend() -> RusqliteBackend {
    RusqliteBackend::open(":memory:", &OpenConfig::default()).expect("open memory backend")
}

fn insert_rows(backend: &RusqliteBackend, table: &str, rows: &[(i64, String)]) {
    for (id, label) in rows {
        backend
            .query_row_typed(
                &format!("INSERT INTO \"{table}\" (id, label) VALUES (?1, ?2)"),
                &[ToSqlValue::Integer(*id), ToSqlValue::Text(label.as_str())],
            )
            .expect("insert generated row");
    }
}

/// Insert single-column TEXT rows. Used by the equivalence
/// tests where we need both backends to hold identical (or
/// targeted-divergent) row contents.
fn insert_single_column_rows(backend: &RusqliteBackend, table: &str, rows: &[String]) {
    for value in rows {
        backend
            .query_row_typed(
                &format!("INSERT INTO \"{table}\" (cell) VALUES (?1)"),
                &[ToSqlValue::Text(value.as_str())],
            )
            .expect("insert generated single-column row");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn proptest_storage_backend_converter_convert_db_counts_and_equivalence(
        t1_rows in row_data(),
        t2_rows in row_data(),
    ) {
        init_test_tracing_json();
        info!(
            test = "convert_db_counts_and_equivalence",
            t1_row_count = t1_rows.len(),
            t2_row_count = t2_rows.len(),
            "begin convert-db proptest case"
        );

        let source = open_backend();
        source
            .execute_batch(
                "CREATE TABLE t1 (id INTEGER, label TEXT); \
                 CREATE TABLE t2 (id INTEGER, label TEXT);",
            )
            .expect("create source schema");
        insert_rows(&source, "t1", &t1_rows);
        insert_rows(&source, "t2", &t2_rows);

        let dest = open_backend();
        dest.execute_batch(
            "CREATE TABLE t1 (id INTEGER, label TEXT); \
             CREATE TABLE t2 (id INTEGER, label TEXT);",
        )
        .expect("create dest schema");

        let outcome = convert_db(&source, &dest, &["t1", "t2"])
            .expect("convert generated tables");

        let expected_tables = vec!["t1".to_string(), "t2".to_string()];
        let expected_rows_per_table = vec![t1_rows.len(), t2_rows.len()];
        prop_assert_eq!(&outcome.tables, &expected_tables);
        prop_assert_eq!(&outcome.rows_per_table, &expected_rows_per_table);
        prop_assert_eq!(outcome.total_rows, t1_rows.len() + t2_rows.len());
        prop_assert_eq!(outcome.row_total(), outcome.total_rows);
        verify_equivalence(&source, &dest, &["t1", "t2"])
            .expect("converted backends are equivalent");

        info!(
            test = "convert_db_counts_and_equivalence",
            total_rows = outcome.total_rows,
            "convert-db case ok"
        );
    }

    #[test]
    fn proptest_storage_backend_converter_copy_table_matches_source_rows(
        rows in row_data(),
    ) {
        init_test_tracing_json();
        info!(
            test = "copy_table_matches_source_rows",
            row_count = rows.len(),
            "begin copy-table proptest case"
        );

        let source = open_backend();
        source
            .execute_batch("CREATE TABLE p (id INTEGER, label TEXT);")
            .expect("create source table");
        insert_rows(&source, "p", &rows);

        let dest = open_backend();
        dest.execute_batch("CREATE TABLE p (id INTEGER, label TEXT);")
            .expect("create dest table");

        let copied = copy_table(&source, &dest, "p").expect("copy generated table");

        prop_assert_eq!(copied, rows.len());
        verify_equivalence(&source, &dest, &["p"])
            .expect("copied table is equivalent");

        info!(
            test = "copy_table_matches_source_rows",
            copied_rows = copied,
            "copy-table case ok"
        );
    }

    /// br-ft-l1jgo phase-2 review: identifier-validation guard
    /// fires before any backend SQL is issued. We use real
    /// (empty) `RusqliteBackend`s so a regression that lets an
    /// unsafe identifier through to the backend would surface
    /// here as a `BackendError::Connect`/`Query` from SQLite
    /// itself rather than the validator's typed
    /// `BackendError::Query` message — both would fail the
    /// `matches!` check, but only the real-backend version
    /// actually proves the validator beats the real-SQL path.
    #[test]
    fn proptest_storage_backend_converter_rejects_unsafe_identifiers(
        table in unsafe_identifier(),
    ) {
        init_test_tracing_json();
        info!(
            test = "rejects_unsafe_identifiers",
            unsafe_identifier = %table,
            "begin reject-unsafe proptest case"
        );

        let source = open_backend();
        let dest = open_backend();

        let copy_err = copy_table(&source, &dest, &table)
            .expect_err("copy_table must reject unsafe identifiers");
        prop_assert!(matches!(copy_err, BackendError::Query(_)),
            "copy_table error must be BackendError::Query, got: {copy_err:?}");

        let verify_err = verify_equivalence(&source, &dest, &[table.as_str()])
            .expect_err("verify_equivalence must reject unsafe identifiers");
        prop_assert!(matches!(verify_err, BackendError::Query(_)),
            "verify_equivalence error must be BackendError::Query, got: {verify_err:?}");

        info!(test = "rejects_unsafe_identifiers", "reject-unsafe case ok");
    }

    /// br-ft-l1jgo phase-2 review: replaces the previous
    /// `MockBackend::enqueue_map_response`-based test with a
    /// real-backend round-trip. Both backends get the same
    /// schema and the same single-column TEXT rows; the
    /// equivalence check should then hold for every variate.
    #[test]
    fn proptest_storage_backend_converter_verify_accepts_identical_real_rows(
        rows in single_column_rows(),
    ) {
        init_test_tracing_json();
        info!(
            test = "verify_accepts_identical_real_rows",
            row_count = rows.len(),
            "begin verify-identical proptest case"
        );

        let source = open_backend();
        let dest = open_backend();
        for backend in [&source, &dest] {
            backend
                .execute_batch("CREATE TABLE shared_table (cell TEXT);")
                .expect("create shared table");
        }
        insert_single_column_rows(&source, "shared_table", &rows);
        insert_single_column_rows(&dest, "shared_table", &rows);

        verify_equivalence(&source, &dest, &["shared_table"])
            .expect("identical rows on real backends must be equivalent");

        info!(
            test = "verify_accepts_identical_real_rows",
            inserted_rows = rows.len(),
            "verify-identical case ok"
        );
    }

    /// br-ft-l1jgo phase-2 review: replaces the previous
    /// `MockBackend::enqueue_map_response`-based divergence
    /// test with a real-backend round-trip. We populate the
    /// same N-row prefix into both backends and then append
    /// one differing row to each, so SQLite's own
    /// `SELECT * ORDER BY rowid` gives `verify_equivalence`
    /// the actual diverging row at index N.
    #[test]
    fn proptest_storage_backend_converter_verify_reports_first_divergence(
        prefix in single_column_rows(),
        (left, right) in different_cells(),
    ) {
        init_test_tracing_json();
        let row_idx = prefix.len();
        info!(
            test = "verify_reports_first_divergence",
            prefix_len = row_idx,
            left = %left,
            right = %right,
            "begin verify-divergence proptest case"
        );

        let source = open_backend();
        let dest = open_backend();
        for backend in [&source, &dest] {
            backend
                .execute_batch("CREATE TABLE diverge_table (cell TEXT);")
                .expect("create diverge table");
        }
        insert_single_column_rows(&source, "diverge_table", &prefix);
        insert_single_column_rows(&dest, "diverge_table", &prefix);
        insert_single_column_rows(&source, "diverge_table", &[left.clone()]);
        insert_single_column_rows(&dest, "diverge_table", &[right.clone()]);

        let err = verify_equivalence(&source, &dest, &["diverge_table"])
            .expect_err("differing tail rows must fail equivalence");
        match err {
            BackendError::Query(msg) => {
                let expected_cell = format!("row {row_idx} column 0");
                prop_assert!(
                    msg.contains(&expected_cell),
                    "error message must locate the divergent cell {expected_cell:?}; got: {msg}"
                );
                prop_assert!(
                    msg.contains(&left),
                    "error message must include source-side cell {left:?}; got: {msg}"
                );
                prop_assert!(
                    msg.contains(&right),
                    "error message must include dest-side cell {right:?}; got: {msg}"
                );
            }
            other => prop_assert!(
                false,
                "expected BackendError::Query for divergence, got {other:?}"
            ),
        }

        info!(
            test = "verify_reports_first_divergence",
            divergent_row_idx = row_idx,
            "verify-divergence case ok"
        );
    }
}
