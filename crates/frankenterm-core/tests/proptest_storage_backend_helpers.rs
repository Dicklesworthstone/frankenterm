//! Property tests for `storage_backend_helpers` against the real
//! `RusqliteBackend` substrate — no mocks, in-memory SQLite.
//!
//! The helpers ship under `storage_backend_helpers` (count_table,
//! count_table_where, row_exists_where, table_exists, pragma_value,
//! max_column, list_user_tables, execute_typed). Each has unit
//! tests in the source-side `mod tests` block, but no property
//! coverage existed for the partition / monotonicity / round-trip
//! invariants that are the actual value the helpers add over the
//! raw rusqlite recipes.
//!
//! This file pins those invariants:
//!
//! 1. `count_table` is monotonic in inserts: inserting N rows and
//!    re-counting gives `count_before + N`.
//! 2. `count_table_where(clause)` partitions: for any clause `c`,
//!    `count_table_where(c) + count_table_where(NOT c) ≤ count_table`
//!    (NULL semantics keep this `≤` rather than `=`).
//! 3. `row_exists_where(clause) == (count_table_where(clause) >= 1)`.
//! 4. `table_exists(name)` ⇔ name appears in `list_user_tables()`.
//! 5. `max_column(t, c)` equals the max of the inserted values, or
//!    `None` on empty tables.
//! 6. `execute_typed(INSERT)` increases `count_table` by exactly 1
//!    (single-row INSERT).
//!
//! Logs are emitted as structured tracing-json events so a
//! failing case lands a parseable record of the inserted rows
//! and the asserted invariant — same shape as
//! `proptest_storage_backend_converter` (br-ft-l1jgo phase-2).

use std::sync::Once;

use frankenterm_core::storage_backend_helpers::{
    count_table, count_table_where, execute_typed, list_user_tables, max_column, pragma_value,
    row_exists_where, table_exists,
};
use frankenterm_core::storage_backend_trait::{
    BackendError, OpenConfig, RusqliteBackend, SQLITE_USER_VERSION_MAX, StorageBackend, ToSqlValue,
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

fn create_id_table(backend: &RusqliteBackend, table: &str) {
    backend
        .execute_batch(&format!(
            "CREATE TABLE \"{table}\" (id INTEGER NOT NULL, label TEXT);"
        ))
        .expect("create table");
}

fn insert_rows(backend: &RusqliteBackend, table: &str, rows: &[(i64, String)]) {
    for (id, label) in rows {
        backend
            .query_row_typed(
                &format!("INSERT INTO \"{table}\" (id, label) VALUES (?1, ?2)"),
                &[ToSqlValue::Integer(*id), ToSqlValue::Text(label.as_str())],
            )
            .expect("insert row");
    }
}

fn safe_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/-]{0,32}".prop_map(String::from)
}

fn id_label_rows() -> impl Strategy<Value = Vec<(i64, String)>> {
    prop::collection::vec((any::<i32>().prop_map(i64::from), safe_text()), 0..16)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// br-ft-l1jgo phase-3 property: count_table is monotonic in
    /// inserts. After N inserts, the count equals the prior count
    /// plus N. Holds even when N == 0 (no inserts → no change).
    #[test]
    fn proptest_storage_backend_helpers_count_table_monotonic(rows in id_label_rows()) {
        init_test_tracing_json();
        let backend = open_backend();
        create_id_table(&backend, "items");

        let before = count_table(&backend, "items").expect("count empty");
        insert_rows(&backend, "items", &rows);
        let after = count_table(&backend, "items").expect("count after inserts");

        info!(
            test = "count_table_monotonic",
            inserted = rows.len(),
            before,
            after,
            "count-table monotonicity case"
        );

        prop_assert_eq!(
            after,
            before + rows.len() as i64,
            "count_table must increase by exactly the inserted-row count"
        );
    }

    /// br-ft-l1jgo phase-3 property: count_table_where partitions
    /// by a discriminator column. For a 2-bucket partition by
    /// `id % 2`, `count_table_where("id % 2 = 0")` +
    /// `count_table_where("id % 2 = 1")` == count_table.
    #[test]
    fn proptest_storage_backend_helpers_count_table_where_partitions(
        rows in id_label_rows()
    ) {
        init_test_tracing_json();
        let backend = open_backend();
        create_id_table(&backend, "items");
        insert_rows(&backend, "items", &rows);

        let total = count_table(&backend, "items").expect("count");
        let evens = count_table_where(&backend, "items", "id % 2 = 0", &[])
            .expect("count evens");
        let odds = count_table_where(&backend, "items", "id % 2 = 1", &[])
            .expect("count odds");
        // Negative IDs in SQLite: `-3 % 2 = -1`, neither 0 nor 1.
        // So evens + odds ≤ total, with equality only when the
        // sample is all non-negative.
        let negatives = count_table_where(&backend, "items", "id % 2 = -1", &[])
            .expect("count negative-mod");

        info!(
            test = "count_table_where_partitions",
            total,
            evens,
            odds,
            negatives,
            "partition-counting case"
        );

        prop_assert_eq!(
            evens + odds + negatives,
            total,
            "{{evens, odds, negative-mod}} must partition the row set"
        );
        prop_assert!(evens >= 0 && odds >= 0 && negatives >= 0);
    }

    /// br-ft-l1jgo phase-3 property: row_exists_where(clause) is
    /// the boolean of (count_table_where(clause) >= 1).
    #[test]
    fn proptest_storage_backend_helpers_row_exists_where_matches_count(
        rows in id_label_rows(),
        target in any::<i32>().prop_map(i64::from),
    ) {
        init_test_tracing_json();
        let backend = open_backend();
        create_id_table(&backend, "items");
        insert_rows(&backend, "items", &rows);

        let exists = row_exists_where(&backend, "items", "id = ?1", &[ToSqlValue::Integer(target)])
            .expect("row exists");
        let count = count_table_where(&backend, "items", "id = ?1", &[ToSqlValue::Integer(target)])
            .expect("count match");

        info!(
            test = "row_exists_where_matches_count",
            target,
            exists,
            count,
            "row-exists vs count-match case"
        );

        prop_assert_eq!(
            exists,
            count >= 1,
            "row_exists_where must equal (count_table_where >= 1)"
        );
    }

    /// br-ft-l1jgo phase-3 property: table_exists(name) iff name
    /// appears in list_user_tables(). Both directions: any table
    /// we created is in the list and reports exists; a non-created
    /// name is neither.
    #[test]
    fn proptest_storage_backend_helpers_table_exists_iff_in_list(
        names in prop::collection::hash_set("[a-z][a-z0-9_]{0,8}", 0..6),
        absent in "[a-z][a-z0-9_]{0,8}",
    ) {
        init_test_tracing_json();
        let backend = open_backend();
        for name in &names {
            backend
                .execute_batch(&format!("CREATE TABLE \"{name}\" (n INTEGER);"))
                .expect("create");
        }

        let listed = list_user_tables(&backend).expect("list");
        let mut listed_sorted = listed.clone();
        listed_sorted.sort();

        info!(
            test = "table_exists_iff_in_list",
            created = ?names,
            listed = ?listed_sorted,
            absent = %absent,
            "table-exists / list-tables case"
        );

        // Every created name must report as exists + appear in list.
        for name in &names {
            prop_assert!(
                table_exists(&backend, name).expect("exists"),
                "{name:?}: created → table_exists must be true"
            );
            prop_assert!(
                listed.iter().any(|n| n == name),
                "{name:?}: created → must appear in list_user_tables"
            );
        }
        // The absent name must not be in either side, unless the
        // proptest happened to also generate it via `names`.
        if !names.contains(&absent) {
            prop_assert!(
                !table_exists(&backend, &absent).expect("exists"),
                "{absent:?}: not created → table_exists must be false"
            );
            prop_assert!(
                !listed.iter().any(|n| n == &absent),
                "{absent:?}: not created → must NOT appear in list_user_tables"
            );
        }
    }

    /// br-ft-l1jgo phase-3 property: max_column(table, col) equals
    /// the max of inserted values, or None if the table is empty.
    #[test]
    fn proptest_storage_backend_helpers_max_column_matches_inserts(
        rows in id_label_rows(),
    ) {
        init_test_tracing_json();
        let backend = open_backend();
        create_id_table(&backend, "items");
        insert_rows(&backend, "items", &rows);

        // max_column returns Option<String> (the typed cell
        // canonical-stringified), so compare against the
        // stringified Iterator::max for round-trip equivalence.
        let max = max_column(&backend, "items", "id").expect("max");
        let expected: Option<String> = rows
            .iter()
            .map(|(id, _)| *id)
            .max()
            .map(|v| v.to_string());

        info!(
            test = "max_column_matches_inserts",
            inserted = rows.len(),
            max = ?max,
            expected = ?expected,
            "max-column case"
        );

        prop_assert_eq!(max, expected, "max_column must equal Iterator::max of inserts");
    }

    /// br-ft-l1jgo phase-3 property: execute_typed(INSERT) routed
    /// through the trait increases count_table by exactly 1.
    #[test]
    fn proptest_storage_backend_helpers_execute_typed_insert_count_increments(
        id in any::<i32>().prop_map(i64::from),
        label in safe_text(),
    ) {
        init_test_tracing_json();
        let backend = open_backend();
        create_id_table(&backend, "items");

        let before = count_table(&backend, "items").expect("count before");
        execute_typed(
            &backend,
            "INSERT INTO items (id, label) VALUES (?1, ?2)",
            &[ToSqlValue::Integer(id), ToSqlValue::Text(label.as_str())],
        )
        .expect("execute_typed insert");
        let after = count_table(&backend, "items").expect("count after");

        info!(
            test = "execute_typed_insert_count_increments",
            id,
            label = %label,
            before,
            after,
            "execute_typed insert case"
        );

        prop_assert_eq!(
            after,
            before + 1,
            "execute_typed INSERT must add exactly one row"
        );
    }

    /// br-ft-l1jgo phase-3 property: pragma_value round-trips for
    /// `user_version`, the only standard SQLite per-DB pragma the
    /// helpers expose by name. Round-trip via the trait's
    /// `set_user_version` setter (the read side via pragma_value
    /// is what the helper covers).
    ///
    /// SQLite's `PRAGMA user_version` is internally a signed
    /// 32-bit integer. Values in that range must round-trip; larger
    /// `u32` values must be rejected by the trait before SQLite can
    /// silently truncate them.
    #[test]
    fn proptest_storage_backend_helpers_pragma_value_round_trips_user_version(
        target in any::<u32>(),
    ) {
        init_test_tracing_json();
        let backend = open_backend();

        if target > SQLITE_USER_VERSION_MAX {
            let err = backend
                .set_user_version(target)
                .expect_err("out-of-range user_version must be rejected");
            prop_assert!(matches!(err, BackendError::Other(_)));
            let read = pragma_value(&backend, "user_version")
                .expect("pragma user_version")
                .expect("user_version always returns a row");
            prop_assert_eq!(
                read,
                "0",
                "rejected user_version must not mutate the pragma"
            );
            return Ok(());
        }

        backend
            .set_user_version(target)
            .expect("set user_version");
        let read = pragma_value(&backend, "user_version")
            .expect("pragma user_version")
            .expect("user_version always returns a row");

        info!(
            test = "pragma_value_round_trips_user_version",
            target,
            read = %read,
            "pragma round-trip case"
        );

        let parsed: u32 = read.parse().expect("user_version parses as u32");
        prop_assert_eq!(parsed, target, "pragma_value must echo set_user_version");
    }
}
