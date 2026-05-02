use std::collections::VecDeque;
use std::sync::Mutex;

use frankenterm_core::storage_backend_converter::{convert_db, copy_table, verify_equivalence};
use frankenterm_core::storage_backend_trait::{
    BackendError, OpenConfig, RusqliteBackend, StorageBackend, ToSqlValue,
};
use proptest::prelude::*;

#[derive(Default)]
struct MockBackend {
    map_responses: Mutex<VecDeque<Vec<Vec<String>>>>,
    queries: Mutex<Vec<(String, Vec<String>)>>,
}

impl MockBackend {
    fn new() -> Self {
        Self::default()
    }

    fn enqueue_map_response(&self, response: Vec<Vec<String>>) {
        self.map_responses.lock().unwrap().push_back(response);
    }

    fn observed_queries(&self) -> Vec<(String, Vec<String>)> {
        self.queries.lock().unwrap().clone()
    }
}

impl StorageBackend for MockBackend {
    fn execute(&self, _sql: &str) -> Result<usize, BackendError> {
        Ok(0)
    }

    fn execute_batch(&self, _sql: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn query_scalar(&self, _sql: &str) -> Result<Option<String>, BackendError> {
        Ok(None)
    }

    fn begin_transaction(
        &self,
    ) -> Result<frankenterm_core::storage_backend_trait::TransactionGuard<'_>, BackendError> {
        Err(BackendError::Other(
            "converter test backend does not support transactions".to_string(),
        ))
    }

    fn user_version(&self) -> Result<u32, BackendError> {
        Ok(0)
    }

    fn set_user_version(&self, _version: u32) -> Result<(), BackendError> {
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "converter_test"
    }

    fn query_map_strings(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<String>>, BackendError> {
        self.queries.lock().unwrap().push((
            sql.to_string(),
            params.iter().map(|param| (*param).to_string()).collect(),
        ));
        Ok(self
            .map_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
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

fn mock_rows() -> impl Strategy<Value = Vec<Vec<String>>> {
    prop::collection::vec(prop::collection::vec(safe_text(), 0..6), 0..12)
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn proptest_storage_backend_converter_convert_db_counts_and_equivalence(
        t1_rows in row_data(),
        t2_rows in row_data(),
    ) {
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

        prop_assert_eq!(outcome.tables, vec!["t1".to_string(), "t2".to_string()]);
        prop_assert_eq!(outcome.rows_per_table, vec![t1_rows.len(), t2_rows.len()]);
        prop_assert_eq!(outcome.total_rows, t1_rows.len() + t2_rows.len());
        prop_assert_eq!(outcome.row_total(), outcome.total_rows);
        verify_equivalence(&source, &dest, &["t1", "t2"])
            .expect("converted backends are equivalent");
    }

    #[test]
    fn proptest_storage_backend_converter_copy_table_matches_source_rows(
        rows in row_data(),
    ) {
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
    }

    #[test]
    fn proptest_storage_backend_converter_rejects_unsafe_identifiers(
        table in unsafe_identifier(),
    ) {
        let source = MockBackend::new();
        let dest = MockBackend::new();

        prop_assert!(matches!(copy_table(&source, &dest, &table), Err(BackendError::Query(_))));
        prop_assert!(matches!(
            verify_equivalence(&source, &dest, &[table.as_str()]),
            Err(BackendError::Query(_))
        ));
    }

    #[test]
    fn proptest_storage_backend_converter_verify_accepts_identical_mock_rows(
        rows in mock_rows(),
    ) {
        let source = MockBackend::new();
        let dest = MockBackend::new();
        source.enqueue_map_response(rows.clone());
        dest.enqueue_map_response(rows);

        verify_equivalence(&source, &dest, &["safe_table"])
            .expect("identical mock rows are equivalent");

        let source_queries = source.observed_queries();
        let dest_queries = dest.observed_queries();
        prop_assert_eq!(source_queries.len(), 1);
        prop_assert_eq!(dest_queries.len(), 1);
        prop_assert_eq!(source_queries[0].0, "SELECT * FROM \"safe_table\" ORDER BY rowid");
        prop_assert_eq!(dest_queries[0].0, source_queries[0].0);
    }

    #[test]
    fn proptest_storage_backend_converter_verify_reports_first_divergence(
        prefix in prop::collection::vec(mock_rows(), 0..3),
        (left, right) in different_cells(),
    ) {
        let row_idx = prefix.len();
        let mut rows_a: Vec<Vec<String>> = prefix
            .into_iter()
            .map(|rows| rows.into_iter().next().unwrap_or_default())
            .collect();
        let mut rows_b = rows_a.clone();
        rows_a.push(vec![left.clone()]);
        rows_b.push(vec![right.clone()]);

        let source = MockBackend::new();
        let dest = MockBackend::new();
        source.enqueue_map_response(rows_a);
        dest.enqueue_map_response(rows_b);

        let err = verify_equivalence(&source, &dest, &["safe_table"])
            .expect_err("different cells must fail equivalence");
        match err {
            BackendError::Query(msg) => {
                prop_assert!(msg.contains(&format!("row {row_idx} column 0")));
                prop_assert!(msg.contains(&left));
                prop_assert!(msg.contains(&right));
            }
            other => prop_assert!(false, "expected query error, got {other:?}"),
        }
    }
}
